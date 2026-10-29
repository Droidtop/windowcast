package dev.windowcast

import java.io.Closeable

/** Where session events come from: a [WindowcastSession], or a fake in a test. */
interface EventSource {
    val isOpen: Boolean

    /** The next event, or null after [timeoutMs] with none. */
    fun nextEvent(timeoutMs: Int): Event?
}

/**
 * What a client does with each event of a session. Every method does nothing unless overridden, so a
 * client implements the ones it shows. Called on the pump's own thread, in the order the events came:
 * a client with a UI posts to its UI thread.
 */
interface SessionListener {
    fun onWindows(windows: List<WindowInfo>) {}
    fun onStreamStarted(event: Event.StreamStarted) {}
    fun onStreamRefused(event: Event.StreamRefused) {}
    fun onStreamStopped(event: Event.StreamStopped) {}
    fun onCarrierStarted(event: Event.CarrierStarted) {}
    fun onCarrierRefused(event: Event.CarrierRefused) {}
    fun onWindowResized(event: Event.WindowResized) {}
    fun onWindowFocused(event: Event.WindowFocused) {}
    fun onHostState(event: Event.HostState) {}
    fun onPrivateSessionRefused(event: Event.PrivateSessionRefused) {}
    fun onCompatibility(event: Event.Compatibility) {}
    fun onCursor(event: Event.Cursor) {}
    fun onClipboard(text: String) {}
    fun onStreamQuality(event: Event.StreamQuality) {}

    /** The session ended. Not called when the pump was closed first. */
    fun onClosed() {}
}

/** Hands [event] to the method of [listener] that is for it. */
fun deliver(event: Event, listener: SessionListener) {
    when (event) {
        is Event.Windows -> listener.onWindows(event.windows)
        is Event.StreamStarted -> listener.onStreamStarted(event)
        is Event.StreamRefused -> listener.onStreamRefused(event)
        is Event.StreamStopped -> listener.onStreamStopped(event)
        is Event.CarrierStarted -> listener.onCarrierStarted(event)
        is Event.CarrierRefused -> listener.onCarrierRefused(event)
        is Event.WindowResized -> listener.onWindowResized(event)
        is Event.WindowFocused -> listener.onWindowFocused(event)
        is Event.HostState -> listener.onHostState(event)
        is Event.PrivateSessionRefused -> listener.onPrivateSessionRefused(event)
        is Event.Compatibility -> listener.onCompatibility(event)
        is Event.Cursor -> listener.onCursor(event)
        is Event.Clipboard -> listener.onClipboard(event.text)
        is Event.StreamQuality -> listener.onStreamQuality(event)
        Event.Closed -> listener.onClosed()
    }
}

/**
 * Reads [source] on a thread of its own and delivers each event to [listener]. This is the one way a
 * client follows a session, so the viewer and droidtop see events the same way. It ends when [close]
 * is called, when the source closes, or after [Event.Closed] (which it delivers, unless it was closed
 * first).
 */
class EventPump(
    private val source: EventSource,
    private val listener: SessionListener,
    private val pollMs: Int = 500,
) : Closeable {
    @Volatile private var running = false
    private var thread: Thread? = null

    /** Starts reading. Once only. */
    @Synchronized
    fun start(): EventPump {
        check(thread == null) { "already started" }
        running = true
        thread = Thread({
            while (running && source.isOpen) {
                val event = source.nextEvent(pollMs) ?: continue
                if (!running) break
                deliver(event, listener)
                if (event == Event.Closed) break
            }
        }, "windowcast-events").also { it.start() }
        return this
    }

    /** Stops delivering at once; the thread ends within one poll. Safe on any thread. */
    fun stop() {
        running = false
    }

    /** Waits up to [timeoutMs] for the thread to end, unless called from the thread itself (a listener closing its own pump). */
    fun join(timeoutMs: Long = JOIN_MS) {
        val t = thread ?: return
        if (t !== Thread.currentThread()) t.join(timeoutMs)
    }

    /** [stop] and [join]. */
    override fun close() {
        stop()
        join()
    }

    private companion object {
        const val JOIN_MS = 2_000L
    }
}
