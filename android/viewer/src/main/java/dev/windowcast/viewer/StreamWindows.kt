package dev.windowcast.viewer

import android.app.Activity
import android.app.Dialog
import android.content.Context
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import dev.windowcast.AudioPlayer
import dev.windowcast.Event
import dev.windowcast.Input
import dev.windowcast.WatchSet
import dev.windowcast.WindowInfo
import dev.windowcast.WindowLayout
import dev.windowcast.WindowRenderer
import dev.windowcast.WindowRenderers
import dev.windowcast.WindowcastSession
import java.util.concurrent.ExecutorService

/**
 * The windows the viewer shows in floating windows of their own, beside the one that fills the
 * screen (Droidtop/tracker#760): a watched window's popups (its dialogs, menus and tooltips, placed
 * against their owner) and the other windows the person chose to watch too, each with its own
 * surface, renderer and, for watched windows, sound. [MainActivity] owns the session and the
 * full-screen window and asks this for the rest. Main thread, except what goes to [worker].
 */
internal class StreamWindows(
    private val activity: Activity,
    private val worker: ExecutorService,
    private val watchSet: WatchSet,
    private val host: Host,
) {
    /** What the floating windows need from the screen around them. */
    interface Host {
        val session: WindowcastSession?

        /** The host's windows as last listed. */
        val windows: List<WindowInfo>

        /** The window filling the screen, and the surface it is drawn on while it shows. */
        val watching: WindowInfo?
        val ownerSurface: SurfaceView?

        fun touch(window: Long, view: View, event: MotionEvent): Boolean

        fun send(input: Input)

        /** A key typed while a floating window is up; true when it went to the host. */
        fun streamKey(event: KeyEvent): Boolean
    }

    /** A floating window and the renderer drawing into it now. */
    private open class Floating(val dialog: Dialog) {
        var renderer: WindowRenderer? = null
    }

    /** A shown window's dialog, popup or menu. */
    private class Popup(dialog: Dialog) : Floating(dialog)

    /**
     * A window watched beside the one that fills the screen: kept until it is closed, the host
     * closes the window, or the session ends.
     */
    private class Extra(dialog: Dialog) : Floating(dialog) {
        var audio: AudioPlayer? = null
    }

    private val popups = mutableMapOf<Long, Popup>()
    private val extras = mutableMapOf<Long, Extra>()

    /** Whether any window is watched beside the full-screen one. */
    val anyExtra: Boolean get() = extras.isNotEmpty()

    /** The windows watched beside the full-screen one. */
    val extraIds: List<Long> get() = extras.keys.toList()

    /** A stream started for a window that is not the full-screen one: an extra, or a popup. */
    fun opened(event: Event.StreamStarted) {
        if (watchSet.isExtra(event.window)) openExtra(event) else openPopup(event)
    }

    /** The host stopped [window]; true when it was one of these. */
    fun stopped(window: Long): Boolean = when {
        popups.containsKey(window) -> {
            closePopup(window)
            true
        }
        extras.containsKey(window) -> {
            closeExtra(window, notifyHost = false)
            true
        }
        else -> false
    }

    /** The full-screen window stopped: its popups go with it. */
    fun closePopups() {
        for (window in popups.keys.toList()) closePopup(window)
    }

    /** Closes every extra window: the session they belong to is ending or gone. */
    fun closeExtras(notifyHost: Boolean) {
        for (window in extras.keys.toList()) closeExtra(window, notifyHost)
        watchSet.clear()
    }

    /** The host resized [Event.WindowResized.window]: an extra window follows. */
    fun resized(event: Event.WindowResized) {
        extras[event.window]?.let { size(it.dialog, event.width, event.height) }
    }

    private fun openPopup(event: Event.StreamStarted) {
        val s = host.session ?: return
        val info = host.windows.firstOrNull { it.id == event.window } ?: return
        if (popups.containsKey(event.window)) return
        val view = SurfaceView(activity)
        val dialog = Dialog(activity)
        val popup = Popup(dialog)
        popups[event.window] = popup
        dialog.setTitle("${info.title} (${info.kind})")
        dialog.setContentView(view)
        size(dialog, info.width, info.height)
        placeAgainstOwner(dialog, info)
        view.setOnTouchListener { v, e -> host.touch(event.window, v, e) }
        dialog.setOnDismissListener {
            // Closed here (Back, a tap outside): the host stops sending it.
            if (popups.remove(event.window) != null) {
                popup.renderer?.let { r -> worker.execute { r.stop() } }
                worker.execute { s.stopWindow(event.window) }
            }
        }
        draw(view, s, info, event, popup)
        dialog.show()
    }

    /**
     * Puts a popup's floating window where it is on the host against its owner, as the owner is
     * shown on screen now (the watched window's surface), when the host gives both positions.
     */
    private fun placeAgainstOwner(dialog: Dialog, info: WindowInfo) {
        val owner = host.windows.firstOrNull { it.id == info.owner } ?: return
        val surface = host.ownerSurface ?: return
        if (owner.id != host.watching?.id || surface.visibility != View.VISIBLE) return
        val origin = IntArray(2)
        surface.getLocationOnScreen(origin)
        val box = WindowLayout.againstOwner(info, owner, origin[0], origin[1], surface.width, surface.height) ?: return
        val attributes = dialog.window?.attributes ?: return
        attributes.gravity = android.view.Gravity.TOP or android.view.Gravity.START
        attributes.x = box.x
        attributes.y = box.y
        attributes.width = box.width
        attributes.height = box.height
        dialog.window?.attributes = attributes
    }

    /** The host stopped a popup (it closed): its floating window goes. */
    private fun closePopup(window: Long) {
        val popup = popups.remove(window) ?: return
        popup.renderer?.let { r -> worker.execute { r.stop() } }
        popup.dialog.dismiss()
    }

    /**
     * Opens a window watched beside the one that fills the screen: its own surface, renderer and
     * sound, with buttons to type into it and to close it. It stays until the person closes it
     * (Close, or Back), the host closes the window, or the session ends. It is not closed when the
     * full-screen window is.
     */
    private fun openExtra(event: Event.StreamStarted) {
        val s = host.session ?: return
        val info = host.windows.firstOrNull { it.id == event.window }
        if (info == null) {
            watchSet.extraEnded(event.window)
            return
        }
        if (extras.containsKey(event.window)) return
        val view = StreamSurfaceView(activity) { host.send(Input.text(it)) }
        view.setOnTouchListener { v, e -> host.touch(event.window, v, e) }
        val dialog = object : Dialog(activity) {
            // Keys typed while this window is up go to the host like any others; Back closes it.
            override fun dispatchKeyEvent(e: KeyEvent): Boolean =
                (e.keyCode != KeyEvent.KEYCODE_BACK && host.streamKey(e)) || super.dispatchKeyEvent(e)
        }
        val buttons = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(
                Button(activity).apply {
                    text = "Keyboard"
                    setOnClickListener { showKeyboard(view) }
                },
                WRAP_CONTENT,
                WRAP_CONTENT,
            )
            addView(
                Button(activity).apply {
                    text = "Close"
                    setOnClickListener { dialog.dismiss() }
                },
                WRAP_CONTENT,
                WRAP_CONTENT,
            )
        }
        val content = FrameLayout(activity).apply {
            addView(view, MATCH_PARENT, MATCH_PARENT)
            addView(
                buttons,
                FrameLayout.LayoutParams(
                    WRAP_CONTENT,
                    WRAP_CONTENT,
                    android.view.Gravity.TOP or android.view.Gravity.END,
                ),
            )
        }
        val shown = Extra(dialog)
        extras[event.window] = shown
        dialog.setTitle(info.title)
        dialog.setContentView(content)
        dialog.setCanceledOnTouchOutside(false)
        size(dialog, info.width, info.height)
        // The keyboard covers the bottom of the window instead of pushing it about.
        dialog.window?.setSoftInputMode(android.view.WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        dialog.setOnDismissListener {
            // Closed here (Close, Back): stop it and tell the host. A window the host closed, or
            // one closed with the session, left the map first, so this does nothing for them.
            if (extras.remove(event.window) != null) {
                watchSet.extraEnded(event.window)
                release(shown)
                worker.execute { s.stopWindow(event.window) }
            }
        }
        draw(view, s, info, event, shown)
        shown.audio = AudioPlayer(s, event.window).also { it.start() }
        dialog.show()
    }

    /** Stops what an extra window is running; the window itself is not touched. */
    private fun release(shown: Extra) {
        shown.renderer?.let { r -> worker.execute { r.stop() } }
        shown.renderer = null
        shown.audio?.let { a -> worker.execute { a.stop() } }
        shown.audio = null
    }

    /** Closes one extra window; with [notifyHost] the host is told to stop sending it. */
    private fun closeExtra(window: Long, notifyHost: Boolean) {
        val shown = extras.remove(window) ?: return
        watchSet.extraEnded(window)
        release(shown)
        shown.dialog.dismiss()
        if (notifyHost) host.session?.let { s -> worker.execute { s.stopWindow(window) } }
    }

    /**
     * Starts a renderer on [view]'s surface when it exists and stops the one [shown] holds when it
     * goes (none, when the window was already released). One path for popups and extras.
     */
    private fun draw(view: SurfaceView, s: WindowcastSession, info: WindowInfo, event: Event.StreamStarted, shown: Floating) {
        view.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                shown.renderer = WindowRenderers.create(s, info, event.backend, event.codec, holder).also { it.start() }
            }
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                shown.renderer?.let { r -> worker.execute { r.stop() } }
                shown.renderer = null
            }
        })
    }

    /** Sizes a floating window to a [width] by [height] picture, shrunk to fit the screen. */
    private fun size(dialog: Dialog, width: Int, height: Int) {
        val screen = activity.resources.displayMetrics
        val (w, h) = WindowLayout.floatingSize(width, height, screen.widthPixels, screen.heightPixels)
        dialog.window?.setLayout(w, h)
    }

}

/** Brings up the soft keyboard for [view], a stream's surface. */
internal fun showKeyboard(view: View) {
    view.requestFocus()
    val imm = view.context.getSystemService(Context.INPUT_METHOD_SERVICE) as android.view.inputmethod.InputMethodManager
    imm.showSoftInput(view, 0)
}
