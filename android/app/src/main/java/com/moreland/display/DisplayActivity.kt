// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.content.Context
import android.content.pm.ActivityInfo
import android.graphics.Color
import android.os.Bundle
import android.os.SystemClock
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView

/**
 * Fullscreen host for the decoded stream.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback, VideoStream.ControlListener {

    private lateinit var surfaceView: TouchSurfaceView
    private lateinit var status: TextView
    private var stream: VideoStream? = null

    /**
     * `SurfaceView` that turns touches into reverse-channel messages.
     *
     * Subclassed rather than wired through `setOnTouchListener` because a
     * `SurfaceView` consumes touches on its own surface before they reach
     * the parent.
     */
    private inner class TouchSurfaceView(context: Context) : SurfaceView(context) {

        private var lastMoveMs = 0L

        override fun onTouchEvent(event: MotionEvent): Boolean {
            val stream = this@DisplayActivity.stream ?: return false
            val w = width.toFloat()
            val h = height.toFloat()
            if (w <= 0f || h <= 0f) return false

            val x = (event.x / w).coerceIn(0f, 1f)
            val y = (event.y / h).coerceIn(0f, 1f)

            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    stream.sendTouch(Protocol.TouchAction.DOWN, x, y)
                    lastMoveMs = SystemClock.uptimeMillis()
                }
                MotionEvent.ACTION_MOVE -> {
                    val now = SystemClock.uptimeMillis()
                    if (now - lastMoveMs < MOVE_INTERVAL_MS) return true
                    lastMoveMs = now
                    stream.sendTouch(Protocol.TouchAction.MOVE, x, y)
                }
                MotionEvent.ACTION_UP -> {
                    stream.sendTouch(Protocol.TouchAction.UP, x, y)
                }
                MotionEvent.ACTION_CANCEL -> {
                    stream.sendTouch(Protocol.TouchAction.CANCEL, x, y)
                }
                else -> return true
            }
            return true
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)

        if (BuildConfig.DRAW_OVER_CUTOUT) {
            window.attributes.layoutInDisplayCutoutMode =
            WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }

        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }

        status = TextView(this).apply {
            text = "Waiting for host\u2026\n\nConnect the tablet and start the daemon."
            setTextColor(Color.parseColor("#888888"))
            textSize = 16f
            val pad = (24 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        surfaceView = TouchSurfaceView(this)

        root.addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        root.addView(status, FrameLayout.LayoutParams(MATCH, WRAP))
        setContentView(root)

        stream = VideoStream().also {
            it.controlListener = this
            it.start()
        }
        surfaceView.holder.addCallback(this)
        goImmersive()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) goImmersive()
    }

    @Suppress("DEPRECATION")
    private fun goImmersive() {
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            )
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        stream?.setSurface(holder.surface)
        status.postDelayed(::refreshStatus, 1000)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        stream?.setSurface(holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stream?.setSurface(null)
    }

    override fun onDestroy() {
        stream?.controlListener = null
        stream?.stop()
        stream = null
        super.onDestroy()
    }

    override fun onBrightness(percent: Int) {
        runOnUiThread {
            val value = percent.coerceIn(0, 100) / 100f
            val attrs = window.attributes
            attrs.screenBrightness = value
            window.attributes = attrs
        }
    }

    override fun onRotation(degrees: Int) {
        runOnUiThread {
            val orientation = when (degrees) {
                0 -> ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
                90 -> ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
                180 -> ActivityInfo.SCREEN_ORIENTATION_REVERSE_LANDSCAPE
                270 -> ActivityInfo.SCREEN_ORIENTATION_REVERSE_PORTRAIT
                else -> return@runOnUiThread
            }
            requestedOrientation = orientation
        }
    }

    private fun refreshStatus() {
        val stream = this.stream ?: return
        if (stream.framesDecoded > 0) {
            status.visibility = View.GONE
        } else {
            stream.lastError?.let { status.text = "Waiting for host\u2026\n\nLast error: $it" }
            status.postDelayed(::refreshStatus, 1000)
        }
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
        /**
         * MOVE rate cap.
         *
         * 8 ms is 125 Hz, above any refresh rate the panel negotiates and
         * well above the rate at which a cursor drag looks smooth. The
         * earlier 4 ms cap produced twice the messages for a difference no
         * human perceives, and every message is a write syscall on the
         * device and a dispatch on the host.
         */
        const val MOVE_INTERVAL_MS = 8L
    }
}
