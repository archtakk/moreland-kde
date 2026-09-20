// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException

/**
 * Mirror of `crates/protocol`.
 *
 * Every multi-byte field on the wire is big-endian, which is precisely why
 * [DataInputStream] can be used directly here - no byte-swapping anywhere.
 */
object Protocol {
    const val SOCKET_NAME = "moreland"

    private const val MAGIC = 0x4D524C44 // "MRLD"
    const val VERSION = 3

    const val STREAM_HEADER_LEN = 16
    const val FRAME_HEADER_LEN = 16
    const val CONTROL_LEN = 2

    /** Every device -> host message: one type byte plus eight payload bytes. */
    const val REVERSE_MSG_LEN = 9

    const val MSG_ACK = 0x01
    const val MSG_TOUCH = 0x02

    const val FLAG_KEYFRAME = 0x01
    const val FLAG_CONTROL = 0x02

    /** Refuse absurd frame lengths rather than attempting the allocation. */
    const val MAX_FRAME_BYTES = 8 * 1024 * 1024

    data class StreamHeader(
        val width: Int,
        val height: Int,
        val framerate: Int,
        val mime: String,
    )

    data class FrameHeader(
        val length: Int,
        val ptsNs: Long,
        val keyframe: Boolean,
        val control: Boolean,
    )

    enum class ControlKind(val id: Int) {
        BRIGHTNESS(1),
        ROTATION(2),
        ;

        companion object {
            fun fromId(id: Int): ControlKind? = entries.firstOrNull { it.id == id }
        }
    }

    data class ControlMessage(val kind: ControlKind, val value: Int)

    /** One device -> host touch action. Single contact only. */
    enum class TouchAction(val id: Int) {
        DOWN(0),
        MOVE(1),
        UP(2),
        CANCEL(3),
        ;

        companion object {
            fun fromId(id: Int): TouchAction? = entries.firstOrNull { it.id == id }
        }
    }

    /**
     * One device -> host touch event.
     *
     * [x] and [y] are fixed-point normalized to `[0..65535]`, representing
     * `[0.0 .. 1.0]` across the surface the touch landed on.
     */
    data class TouchMessage(val action: TouchAction, val x: Int, val y: Int)

    @Throws(IOException::class)
    fun readStreamHeader(input: DataInputStream): StreamHeader {
        val magic = input.readInt()
        if (magic != MAGIC) {
            throw IOException("bad magic 0x%08x".format(magic))
        }
        val version = input.readUnsignedShort()
        if (version != VERSION) {
            throw IOException("unsupported protocol version $version (expected $VERSION)")
        }
        val width = input.readUnsignedShort()
        val height = input.readUnsignedShort()
        val framerate = input.readUnsignedShort()
        val codec = input.readUnsignedByte()
        // `skipBytes` is documented to skip *up to* n bytes and return the
        // count actually skipped; it does not throw on a short stream. A
        // 15-byte header would slip through with the reserved bytes missing
        // and the decoder would then be configured from a stream whose
        // framing is already broken. Reject it explicitly.
        if (input.skipBytes(3) != 3) {
            throw IOException("stream header truncated in reserved bytes")
        }

        val mime = when (codec) {
            0 -> "video/avc"
            1 -> "video/hevc"
            else -> throw IOException("unknown codec id $codec")
        }
        return StreamHeader(width, height, framerate, mime)
    }

    @Throws(IOException::class)
    fun readFrameHeader(input: DataInputStream): FrameHeader {
        val length = input.readInt()
        if (length < 0 || length > MAX_FRAME_BYTES) {
            throw IOException("implausible frame length $length")
        }
        val ptsNs = input.readLong()
        val flags = input.readUnsignedByte()
        // Same reason as in `readStreamHeader`: a frame header that is
        // short by the reserved bytes is not a valid header, and silently
        // accepting it would desynchronise the frame loop.
        if (input.skipBytes(3) != 3) {
            throw IOException("frame header truncated in reserved bytes")
        }
        return FrameHeader(
            length = length,
            ptsNs = ptsNs,
            keyframe = flags and FLAG_KEYFRAME != 0,
            control = flags and FLAG_CONTROL != 0,
        )
    }

    @Throws(IOException::class)
    fun decodeControl(buffer: ByteArray, length: Int): ControlMessage {
        if (length < CONTROL_LEN) {
            throw IOException("control message too short: $length")
        }
        val kindId = buffer[0].toInt() and 0xFF
        val kind = ControlKind.fromId(kindId)
            ?: throw IOException("unknown control kind $kindId")
        return ControlMessage(kind, buffer[1].toInt() and 0xFF)
    }

    @Throws(IOException::class)
    fun readFully(input: DataInputStream, buffer: ByteArray, length: Int) {
        var read = 0
        while (read < length) {
            val n = input.read(buffer, read, length - read)
            if (n < 0) throw EOFException("stream closed after $read of $length bytes")
            read += n
        }
    }

    /** Encode an ack into a full reverse message. */
    fun encodeAckMessage(ptsNs: Long): ByteArray {
        val buf = ByteArray(REVERSE_MSG_LEN)
        buf[0] = MSG_ACK.toByte()
        for (i in 0 until 8) {
            buf[1 + i] = (ptsNs ushr (56 - i * 8)).toByte()
        }
        return buf
    }

    /** Encode a touch message into a full reverse message. */
    fun encodeTouchMessage(msg: TouchMessage): ByteArray {
        val buf = ByteArray(REVERSE_MSG_LEN)
        buf[0] = MSG_TOUCH.toByte()
        buf[1] = msg.action.id.toByte()
        buf[2] = 0
        buf[3] = ((msg.x ushr 8) and 0xFF).toByte()
        buf[4] = (msg.x and 0xFF).toByte()
        buf[5] = ((msg.y ushr 8) and 0xFF).toByte()
        buf[6] = (msg.y and 0xFF).toByte()
        buf[7] = 0
        buf[8] = 0
        return buf
    }
}
