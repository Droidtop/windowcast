package dev.windowcast

/**
 * A native handle that is freed only once nobody is inside a call on it. Native calls run
 * outside any lock (they may block, and several threads may use one handle: the native side is
 * safe for that), but [close] never frees under a running call: it marks the handle closed, so
 * later calls get their default, and the last call to leave frees it. [close] itself never
 * waits, so it is safe on the main thread.
 */
internal class NativeHandle(handle: Long, private val free: (Long) -> Unit) {
    private val lock = Any()
    private var handle = handle
    private var users = 0
    private var closing = false

    val isOpen: Boolean get() = synchronized(lock) { !closing && handle != 0L }

    /** Runs [block] with the live handle, or returns [default] once closed. */
    fun <T> use(default: T, block: (Long) -> T): T {
        val h = synchronized(lock) {
            if (closing || handle == 0L) return default
            users++
            handle
        }
        try {
            return block(h)
        } finally {
            val toFree = synchronized(lock) { if (--users == 0) takeIfClosing() else 0L }
            if (toFree != 0L) free(toFree)
        }
    }

    fun close() {
        val toFree = synchronized(lock) {
            closing = true
            if (users == 0) takeIfClosing() else 0L
        }
        if (toFree != 0L) free(toFree)
    }

    private fun takeIfClosing(): Long {
        if (!closing) return 0L
        val h = handle
        handle = 0L
        return h
    }
}
