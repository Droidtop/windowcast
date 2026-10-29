package dev.windowcast

import java.io.Closeable

/** What [SessionHolder] needs of a session: events to follow, windows to ask for, and closing. */
interface HostedSession : EventSource, Closeable {
    /** Asks the host to describe windows as pictures (RDP) where its rules send them so. */
    fun acceptPictures(on: Boolean)

    /** Asks for the window list; the answer is an [Event.Windows]. */
    fun requestWindows()
}

/**
 * The one current session of a client, and the [EventPump] following it. Starting a session is the
 * same sequence for the viewer and for droidtop: stop the old reader and wait for it (it may be inside
 * `nextEvent` on the session about to close, Droidtop/tracker#447), close the old session, open the
 * new one, follow it, ask for its windows. Every call but [session] blocks, so none may run on a UI
 * thread.
 */
class SessionHolder<S : HostedSession> {
    @Volatile var session: S? = null
        private set
    private var pump: EventPump? = null

    /**
     * Replaces the current session with the one [open] makes. [beforeClose] runs first, with the old
     * reader already stopped and the old session still open (a client stops what uses it: its
     * microphone). [opened] runs with the new session before its events are delivered (a client says
     * it is connected). If [open] throws, nothing is left running: the old session is already gone.
     */
    @Synchronized
    fun start(
        listener: SessionListener,
        beforeClose: () -> Unit = {},
        opened: (S) -> Unit = {},
        open: () -> S,
    ): S {
        stopReader()
        beforeClose()
        closeSession()
        val s = open()
        s.acceptPictures(true)
        session = s
        opened(s)
        pump = EventPump(s, listener).start()
        s.requestWindows()
        return s
    }

    /** Follows a session that is already open (one kept across an activity's recreation). */
    @Synchronized
    fun adopt(s: S, listener: SessionListener) {
        stopReader()
        session = s
        pump = EventPump(s, listener).start()
    }

    /**
     * Stops following without closing the session (it is being handed on). [wait] joins the reader,
     * which a UI thread must not do.
     */
    @Synchronized
    fun release(wait: Boolean = true): S? {
        val p = pump
        pump = null
        p?.stop()
        if (wait) p?.join()
        val s = session
        session = null
        return s
    }

    /** Stops following and closes the session. */
    @Synchronized
    fun end() {
        stopReader()
        closeSession()
    }

    private fun stopReader() {
        pump?.close()
        pump = null
    }

    private fun closeSession() {
        session?.close()
        session = null
    }
}
