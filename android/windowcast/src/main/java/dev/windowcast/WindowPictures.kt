package dev.windowcast

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Rect
import android.util.Log
import android.view.SurfaceHolder
import java.nio.ByteBuffer

/**
 * Shows a window streamed over RDP: its pictures come whole as RGBA
 * whenever the window changes, and are drawn onto the surface, stretched
 * to it as a decoded stream is. Runs on its own thread.
 */
class WindowPictures(
    private val session: WindowcastSession,
    private val window: Long,
    private val holder: SurfaceHolder,
    /** Called on the drawing thread with pictures shown so far. */
    private val onProgress: (Stats) -> Unit = {},
) : WindowRenderer {
    data class Stats(val pictures: Long, val width: Int, val height: Int, val ended: Boolean, val error: String?)

    @Volatile private var running = true
    private val thread = Thread(::run, "windowcast-pictures-$window")

    override fun start() = thread.start()

    override fun stop() {
        running = false
        thread.join(2000)
    }

    private fun run() {
        var buffer = ByteBuffer.allocateDirect(1920 * 1080 * 4)
        val size = IntArray(2)
        var bitmap: Bitmap? = null
        var pictures = 0L
        var error: String? = null
        val paint = Paint(Paint.FILTER_BITMAP_FLAG)
        try {
            while (running) {
                val len = session.withHandle(Native.ERROR) { Native.nextPicture(it, window, 200, buffer, size) }
                when {
                    len == Native.TIMEOUT -> continue
                    len == Native.BUFFER_TOO_SMALL -> {
                        buffer = ByteBuffer.allocateDirect(size[0] * size[1] * 4)
                        continue
                    }
                    len < 0 -> break
                }
                val (width, height) = size[0] to size[1]
                if (bitmap == null || bitmap.width != width || bitmap.height != height) {
                    bitmap?.recycle()
                    bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
                }
                // ARGB_8888 is RGBA in memory, the order the pictures come in.
                buffer.position(0)
                buffer.limit(len.toInt())
                bitmap!!.copyPixelsFromBuffer(buffer)
                buffer.clear()
                draw(bitmap, paint)
                pictures++
                if (pictures % 30 == 1L) onProgress(Stats(pictures, width, height, false, null))
            }
        } catch (e: Exception) {
            Log.e(TAG, "showing window $window failed", e)
            error = e.toString()
        } finally {
            bitmap?.recycle()
            onProgress(Stats(pictures, bitmap?.width ?: 0, bitmap?.height ?: 0, true, error))
        }
    }

    private fun draw(bitmap: Bitmap, paint: Paint) {
        val canvas: Canvas = holder.lockCanvas() ?: return
        try {
            // The whole surface, as a decoded stream fills it, so touch
            // positions (fractions of the view) land where they show.
            canvas.drawBitmap(bitmap, null, Rect(0, 0, canvas.width, canvas.height), paint)
        } finally {
            holder.unlockCanvasAndPost(canvas)
        }
    }

    private companion object {
        const val TAG = "windowcast"
    }
}
