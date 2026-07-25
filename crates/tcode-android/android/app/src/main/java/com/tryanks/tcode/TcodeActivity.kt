package com.tryanks.tcode

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.res.Configuration
import android.graphics.PixelFormat
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Choreographer
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.inputmethod.InputMethodManager

/**
 * Host side of the JNI boundary described in `crates/gpui-android/README.md`.
 *
 * Every native call below happens on the main Looper thread. That is not
 * incidental: the Rust event sink is `!Send`, and the surface lifecycle contract
 * requires create/destroy/frame to be serialized against each other.
 */
class TcodeActivity : Activity(), SurfaceHolder.Callback2 {

    companion object {
        init {
            System.loadLibrary("tcode_android")
        }

        // Must match `touch_phase` in crates/tcode-android/src/entry.rs. The
        // Rust side rejects unknown values rather than guessing, so these two
        // lists have to be kept together.
        private const val TOUCH_STARTED = 0
        private const val TOUCH_MOVED = 1
        private const val TOUCH_ENDED = 2
        private const val TOUCH_CANCELLED = 3

        // Must match `lifecycle_phase` in the same file.
        private const val LIFECYCLE_ACTIVE = 0
        private const val LIFECYCLE_INACTIVE = 1
        private const val LIFECYCLE_BACKGROUND = 2
        private const val LIFECYCLE_FOREGROUND = 3
    }

    private lateinit var surfaceView: SurfaceView
    private val handler = Handler(Looper.getMainLooper())
    private var started = false
    private var backEnabled = false

    private external fun nativeStart(surface: Surface, width: Int, height: Int, density: Float)
    private external fun nativeSurfaceCreated(surface: Surface, width: Int, height: Int, density: Float)
    private external fun nativeSurfaceDestroyed()
    private external fun nativeResized(width: Int, height: Int, density: Float)
    private external fun nativeFrame()
    private external fun nativeDrainMainThread()
    private external fun nativeTouch(pointerId: Long, phase: Int, x: Float, y: Float, pressure: Float)
    private external fun nativeLifecycle(phase: Int)
    private external fun nativeBackPressed()
    private external fun nativeMemoryWarning()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        surfaceView = SurfaceView(this)
        surfaceView.holder.setFormat(PixelFormat.RGBA_8888)
        surfaceView.holder.addCallback(this)
        setContentView(surfaceView)
    }

    // -- surface lifecycle ---------------------------------------------------

    override fun surfaceCreated(holder: SurfaceHolder) {
        // Dimensions arrive with surfaceChanged; nothing to report yet.
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        val density = resources.displayMetrics.density
        if (!started) {
            // The first surface must stay valid across GPUI's launch.
            nativeStart(holder.surface, width, height, density)
            started = true
            requestFrame()
        } else {
            nativeSurfaceCreated(holder.surface, width, height, density)
            nativeResized(width, height, density)
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        // Must precede Android releasing the surface: the Rust side unconfigures
        // rendering and holds the retired ANativeWindow lease until a
        // replacement succeeds, so wgpu cannot outlive the window.
        nativeSurfaceDestroyed()
    }

    override fun surfaceRedrawNeeded(holder: SurfaceHolder) {
        nativeFrame()
    }

    // -- frame and main-thread pumping --------------------------------------

    /** Called from Rust (`AndroidHost::request_frame`), already coalesced there. */
    fun requestFrame() {
        Choreographer.getInstance().postFrameCallback {
            nativeFrame()
        }
    }

    /** Called from Rust worker threads (`AndroidHost::wake_main_thread`). */
    fun wakeMainThread() {
        handler.post { nativeDrainMainThread() }
    }

    // -- input --------------------------------------------------------------

    override fun onTouchEvent(event: MotionEvent): Boolean {
        val phase = when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> TOUCH_STARTED
            MotionEvent.ACTION_MOVE -> TOUCH_MOVED
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> TOUCH_ENDED
            MotionEvent.ACTION_CANCEL -> TOUCH_CANCELLED
            else -> return false
        }
        // ACTION_MOVE batches every active pointer into one event, so all of
        // them must be forwarded or a multi-touch gesture loses fingers.
        for (index in 0 until event.pointerCount) {
            nativeTouch(
                event.getPointerId(index).toLong(),
                phase,
                event.getX(index),
                event.getY(index),
                event.getPressure(index),
            )
        }
        return true
    }

    override fun onBackPressed() {
        if (backEnabled) {
            nativeBackPressed()
        } else {
            super.onBackPressed()
        }
    }

    // -- lifecycle ----------------------------------------------------------

    override fun onResume() {
        super.onResume()
        nativeLifecycle(LIFECYCLE_ACTIVE)
    }

    override fun onPause() {
        super.onPause()
        nativeLifecycle(LIFECYCLE_INACTIVE)
    }

    override fun onStart() {
        super.onStart()
        nativeLifecycle(LIFECYCLE_FOREGROUND)
    }

    override fun onStop() {
        super.onStop()
        nativeLifecycle(LIFECYCLE_BACKGROUND)
    }

    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        nativeMemoryWarning()
    }

    // -- AndroidHost operations, called from Rust ---------------------------

    fun finishActivity() = runOnUiThread { finish() }

    fun openUri(uri: String) {
        startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(uri)))
    }

    fun readClipboardText(): String? {
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        return clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
    }

    fun writeClipboardText(text: String) {
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText(null, text))
    }

    fun showSoftKeyboard() = runOnUiThread {
        val ime = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        ime.showSoftInput(surfaceView, 0)
    }

    fun hideSoftKeyboard() = runOnUiThread {
        val ime = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        ime.hideSoftInputFromWindow(surfaceView.windowToken, 0)
    }

    fun setBackEnabled(enabled: Boolean) {
        backEnabled = enabled
    }

    fun isDarkMode(): Boolean =
        (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
            Configuration.UI_MODE_NIGHT_YES
}
