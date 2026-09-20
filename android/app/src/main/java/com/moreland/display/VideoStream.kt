// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.util.Log
import android.view.Surface
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Accepts one host connection at a time on an abstract Unix socket, decodes
 * the H.264 stream, renders to whatever surface is currently attached, and
 * drives the device->host reverse channel (acks and touch).
 */
class VideoStream {

    interface ControlListener {
        fun onBrightness(percent: Int)
        fun onRotation(degrees: Int)
    }

    /**
     * Everything the session thread can push onto the socket.
     *
     * Sealed rather than two parallel queues so the writer has exactly one
     * place to block on, and exactly one thread owns the `OutputStream`. A
     * second writer thread is the one design mistake this shape exists to
     * prevent.
     */
    private sealed class Outbound {
        data class Ack(val ptsNs: Long) : Outbound()
        data class Touch(val message: Protocol.TouchMessage) : Outbound()
        /**
         * Internal sentinel that wakes the writer to emit a pending MOVE.
         * Never written to the socket. Needed because MOVEs bypass the
         * queue entirely — see [sendTouch] — and without a nudge the writer
         * would only notice the pending MOVE on the next ack or on the
         * 500 ms poll timeout. Idle outputs produce no acks, so during a
         * sustained drag on a still screen the timeout would otherwise be
         * the only thing waking it.
         */
        object Wake : Outbound()
    }

    private companion object {
        const val TAG = "Moreland"
        const val INPUT_TIMEOUT_MS = 2000L
        const val SURFACE_WAIT_MS = 5000L
        /**
         * Upper bound on messages per socket write. Larger batches amortise
         * the syscall further but add latency to the newest message in the
         * batch; 128 messages is 1152 bytes, small enough that the extra
         * delay is well under a frame at any refresh rate.
         */
        const val BATCH_MAX_MESSAGES = 128
    }

    @Volatile private var running = false
    @Volatile private var surface: Surface? = null
    @Volatile private var codec: MediaCodec? = null
    private var serverSocket: LocalServerSocket? = null
    private var client: LocalSocket? = null
    private var thread: Thread? = null
    private var writerThread: Thread? = null

    @Volatile var controlListener: ControlListener? = null

    private val freeInputs = LinkedBlockingQueue<Int>()
    private val pendingOutbound = LinkedBlockingQueue<Outbound>()

    /**
     * The most recent MOVE not yet written.
     *
     * MOVEs are not queued. Android fires ACTION_MOVE at the panel's touch
     * sampling rate — 144 Hz here — and every event carries a position that
     * is usually within a pixel of the last one. Enqueuing each of them
     * floods the writer with messages that are individually less useful
     * than the one that arrives a few milliseconds later, and every extra
     * message is a syscall that delays the ack behind it in the writer's
     * queue. Only the latest position matters, so only the latest is kept.
     *
     * DOWN, UP, and CANCEL still go through the queue, because their order
     * relative to each other is the whole gesture. A non-MOVE drains this
     * slot first (see [sendTouch]) so the queue never contains a MOVE that
     * logically belongs after a non-MOVE.
     */
    private val pendingMove = AtomicReference<Protocol.TouchMessage?>(null)

    @Volatile var framesDecoded: Long = 0; private set
    @Volatile var lastError: String? = null; private set

    fun setSurface(surface: Surface?) {
        this.surface = surface
        val codec = this.codec ?: return
        if (surface != null && surface.isValid) {
            runCatching { codec.setOutputSurface(surface) }
                .onFailure { Log.w(TAG, "setOutputSurface failed: ${it.message}") }
        }
    }

    fun start() {
        if (running) return
        running = true
        thread = Thread(::acceptLoop, "moreland-stream").apply { start() }
    }

    fun stop() {
        running = false
        runCatching { client?.close() }
        runCatching { serverSocket?.close() }
        thread?.interrupt()
        writerThread?.interrupt()
        thread = null
        writerThread = null
    }

    /**
     * Queue a touch event for the host. Callable from any thread.
     *
     * A no-op when no session is up: a touch that arrives before a host has
     * connected has nowhere to go, and queueing it would deliver a stale
     * event later.
     */
    fun sendTouch(action: Protocol.TouchAction, xNorm: Float, yNorm: Float) {
        if (!running) return
        val x = (xNorm.coerceIn(0f, 1f) * 65535f + 0.5f).toInt()
        val y = (yNorm.coerceIn(0f, 1f) * 65535f + 0.5f).toInt()
        val msg = Protocol.TouchMessage(action, x, y)

        when (action) {
            Protocol.TouchAction.MOVE -> {
                // Overwrite any pending move. `getAndSet` returns the
                // previous value; if it was null this is the first MOVE
                // since the writer last drained the slot, so nudge the
                // writer awake. Nudging only on the null-to-non-null
                // transition keeps the Wake sentinel from piling up in the
                // queue during a long drag.
                if (pendingMove.getAndSet(msg) == null) {
                    pendingOutbound.offer(Outbound.Wake)
                }
            }
            Protocol.TouchAction.DOWN -> {
                // Drop any stale move before opening the contact. A move
                // that precedes a DOWN has no meaning; on the host it would
                // be ignored anyway, but discarding it here avoids sending
                // a message that could only ever be dropped.
                pendingMove.set(null)
                pendingOutbound.offer(Outbound.Touch(msg))
            }
            Protocol.TouchAction.UP, Protocol.TouchAction.CANCEL -> {
                // Flush the pending move *first*, so the release position
                // recorded by the compositor is the last position the
                // finger actually had, not the last position the writer
                // happened to drain before the UP arrived. Without this,
                // a slow drag's release would land where the finger was a
                // few milliseconds earlier.
                val pm = pendingMove.getAndSet(null)
                if (pm != null) pendingOutbound.offer(Outbound.Touch(pm))
                pendingOutbound.offer(Outbound.Touch(msg))
            }
        }
    }

    private fun acceptLoop() {
        while (running) {
            try {
                LocalServerSocket(Protocol.SOCKET_NAME).use { server ->
                    serverSocket = server
                    Log.i(TAG, "listening on localabstract:${Protocol.SOCKET_NAME}")
                    while (running) {
                        val socket = server.accept()
                        Log.i(TAG, "host connected")
                        client = socket
                        runCatching { session(socket) }
                            .onFailure {
                                lastError = it.message
                                Log.w(TAG, "session ended: ${it.message}")
                            }
                        runCatching { socket.close() }
                        client = null
                    }
                }
            } catch (e: Exception) {
                if (!running) return
                lastError = e.message
                Log.w(TAG, "accept loop error: ${e.message}")
                runCatching { Thread.sleep(500) }
            } finally {
                serverSocket = null
            }
        }
        Log.i(TAG, "accept loop exited")
    }

    private fun awaitSurface(): Surface {
        val deadline = System.currentTimeMillis() + SURFACE_WAIT_MS
        while (System.currentTimeMillis() < deadline) {
            val current = surface
            if (current != null && current.isValid) return current
            Thread.sleep(50)
        }
        throw IOException(
            "no valid surface after ${SURFACE_WAIT_MS}ms - " +
                "is the screen on and this app in the foreground?"
        )
    }

    private fun session(socket: LocalSocket) {
        val input = DataInputStream(socket.inputStream.buffered(256 * 1024))
        val output = socket.outputStream

        val header = Protocol.readStreamHeader(input)
        Log.i(TAG, "stream ${header.width}x${header.height}@${header.framerate} ${header.mime}")

        val target = awaitSurface()
        freeInputs.clear()
        pendingOutbound.clear()

        val codec = configureCodec(header, target)
        this.codec = codec
        startWriter(output)

        try {
            var scratch = ByteArray(256 * 1024)
            while (running) {
                val frame = Protocol.readFrameHeader(input)
                if (frame.length > scratch.size) {
                    scratch = ByteArray(frame.length)
                }
                Protocol.readFully(input, scratch, frame.length)

                if (frame.control) {
                    handleControl(frame, scratch)
                    continue
                }

                val index = freeInputs.poll(INPUT_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                if (index == null) {
                    Log.w(TAG, "decoder starved of input buffers; dropping frame")
                    continue
                }
                val inputBuffer = codec.getInputBuffer(index) ?: continue
                inputBuffer.clear()
                inputBuffer.put(scratch, 0, frame.length)

                val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                codec.queueInputBuffer(index, 0, frame.length, frame.ptsNs / 1000, flags)
            }
        } finally {
            this.codec = null
            runCatching { codec.stop() }
            runCatching { codec.release() }
            writerThread?.interrupt()
            writerThread = null
        }
    }

    private fun handleControl(frame: Protocol.FrameHeader, scratch: ByteArray) {
        val msg = try {
            Protocol.decodeControl(scratch, frame.length)
        } catch (e: IOException) {
            Log.w(TAG, "dropping malformed control message: ${e.message}")
            return
        }
        val listener = controlListener ?: return
        when (msg.kind) {
            Protocol.ControlKind.BRIGHTNESS ->
                listener.onBrightness(msg.value.coerceIn(0, 100))
            Protocol.ControlKind.ROTATION -> {
                val degrees = when (msg.value) {
                    0 -> 0
                    1 -> 90
                    2 -> 180
                    3 -> 270
                    else -> return
                }
                listener.onRotation(degrees)
            }
        }
    }

    /**
     * Whether the decoder that `MediaCodec.createDecoderByType` will
     * select for this stream advertises the low-latency feature.
     *
     * `KEY_LOW_LATENCY` is documented as optional, but not every decoder
     * treats an unimplemented feature as a no-op. `MediaCodecInfo
     * .CodecCapabilities.isFeatureSupported` is the documented way to
     * check. This runs once per session; the codec-list scan is O(number
     * of registered codecs) and happens before the first frame, not in
     * the frame loop.
     */
    private fun supportsLowLatency(header: Protocol.StreamHeader): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return false
        val probe = MediaFormat.createVideoFormat(header.mime, header.width, header.height)
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
        val decoderName = list.findDecoderForFormat(probe) ?: return false
        for (info in list.codecInfos) {
            // `MediaCodecInfo` has `isEncoder()`; a codec that is not
            // an encoder is a decoder.
            if (info.isEncoder || info.name != decoderName) continue
            return try {
                info.getCapabilitiesForType(header.mime)
                    .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
            } catch (_: IllegalArgumentException) {
                // getCapabilitiesForType throws if the MIME is not
                // supported by this codec, which findDecoderForFormat
                // should already have excluded. Treat as "no feature".
                false
            }
        }
        return false
    }

    private fun configureCodec(header: Protocol.StreamHeader, target: Surface): MediaCodec {
        val format = MediaFormat.createVideoFormat(header.mime, header.width, header.height)

        // Ask the decoder whether it implements the low-latency feature
        // before setting the hint. Some decoders — notably Samsung's
        // Exynos series (OMX.Exynos.avc.dec) — reject the entire
        // configure() call with ERROR_UNSUPPORTED (0xfffffc0e) rather than
        // ignoring an unimplemented hint, which fails the session before a
        // single frame is decoded.
        if (supportsLowLatency(header)) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        runCatching { format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1) }

        val codec = MediaCodec.createDecoderByType(header.mime)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                freeInputs.offer(index)
            }

            override fun onOutputBufferAvailable(
                codec: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo,
            ) {
                val render = surface?.isValid == true
                runCatching { codec.releaseOutputBuffer(index, render) }
                if (render) {
                    framesDecoded++
                    pendingOutbound.offer(Outbound.Ack(info.presentationTimeUs * 1000))
                }
            }

            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                lastError = e.message
                Log.e(TAG, "codec error: ${e.message}", e)
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "output format: $format")
            }
        })

        codec.configure(format, target, null, 0)
        codec.start()
        Log.i(TAG, "decoder started: ${codec.name}")
        return codec
    }

    /**
     * The **only** thread that writes to the socket.
     *
     * Acks and touches share this queue and this writer. A second writer
     * thread would interleave partial `write()`s across the 9-byte framing
     * and corrupt both streams at once.
     */
    private fun startWriter(output: OutputStream) {
        writerThread = Thread({
            val batch = ByteArray(Protocol.REVERSE_MSG_LEN * BATCH_MAX_MESSAGES)
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    // Block until something arrives. The 500 ms timeout is
                    // just a liveness check — under normal video flow an ack
                    // wakes this every frame.
                    val first = pendingOutbound.poll(500, TimeUnit.MILLISECONDS) ?: continue

                    var offset = 0
                    offset = appendOrSkip(batch, offset, first)

                    // Drain everything else that is already queued, up to
                    // the batch size. `poll()` (no timeout) is the
                    // non-blocking form: when the queue is empty it returns
                    // null immediately and the loop exits.
                    while (offset + Protocol.REVERSE_MSG_LEN <= batch.size) {
                        val next = pendingOutbound.poll() ?: break
                        offset = appendOrSkip(batch, offset, next)
                    }

                    // Emit the pending MOVE, if any. This is safe to do
                    // *after* the queue drain because `sendTouch` clears
                    // this slot before enqueuing a DOWN/UP/CANCEL, so
                    // anything left here logically follows everything the
                    // queue just produced.
                    val pm = pendingMove.getAndSet(null)
                    if (pm != null && offset + Protocol.REVERSE_MSG_LEN <= batch.size) {
                        offset += appendTouch(batch, offset, pm)
                    }

                    if (offset > 0) {
                        output.write(batch, 0, offset)
                        output.flush()
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "writer stopped: ${e.message}")
            }
        }, "moreland-out").apply { start() }
    }

    /**
     * Append one queued message to [buf], or skip it if it is the [Outbound.Wake]
     * sentinel. Returns the new offset.
     */
    private fun appendOrSkip(buf: ByteArray, offset: Int, msg: Outbound): Int = when (msg) {
        is Outbound.Ack -> {
            val bytes = Protocol.encodeAckMessage(msg.ptsNs)
            System.arraycopy(bytes, 0, buf, offset, bytes.size)
            offset + bytes.size
        }
        is Outbound.Touch -> appendTouch(buf, offset, msg.message)
        Outbound.Wake -> offset
    }

    private fun appendTouch(buf: ByteArray, offset: Int, msg: Protocol.TouchMessage): Int {
        val bytes = Protocol.encodeTouchMessage(msg)
        System.arraycopy(bytes, 0, buf, offset, bytes.size)
        return offset + bytes.size
    }
}
