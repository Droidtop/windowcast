package dev.windowcast

import android.view.SurfaceHolder

/**
 * Which renderer shows a stream, and the progress it reports in one shape. A window carried as RDP
 * pictures is drawn by [WindowPictures], any other carrier by [WindowDecoder]; the viewer and droidtop
 * choose the same way, from one place.
 */
object WindowRenderers {
    /** What a renderer reports as it runs. */
    data class Progress(
        /** True for RDP pictures, false for decoded video. */
        val pictures: Boolean,
        /** Pictures or frames shown so far. */
        val count: Long,
        /** What was shown, after the count: "pictures over RDP, 800x600" or "frames (H264)". */
        val what: String,
        val ended: Boolean,
        val error: String?,
    ) {
        /** "1234 frames (H264)". */
        val line: String get() = "$count $what"
    }

    /**
     * The renderer for [event]'s stream of [window] onto [holder]; call `start()` on it once the
     * surface is valid. [onProgress] is called on the renderer's own thread.
     */
    fun create(
        session: WindowcastSession,
        event: Event.StreamStarted,
        window: WindowInfo,
        holder: SurfaceHolder,
        onProgress: (Progress) -> Unit = {},
    ): WindowRenderer =
        if (event.backend == "Rdp") {
            WindowPictures(session, event.window, holder) { s ->
                onProgress(Progress(true, s.pictures, "pictures over RDP, ${s.width}x${s.height}", s.ended, s.error))
            }
        } else {
            WindowDecoder(session, event.window, holder.surface, window.width, window.height) { s ->
                onProgress(Progress(false, s.frames, "frames (${s.codec ?: event.codec})", s.ended, s.error))
            }
        }
}
