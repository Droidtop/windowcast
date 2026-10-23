package dev.windowcast.viewer

/**
 * Which windows the viewer is showing, and what a request to watch one should do
 * (docs/ANDROID-VIEWER.md, "Several windows at once").
 *
 * The owner's rule: windowcast streams arbitrary windows, so several at once is intended, and
 * a window stream closes only when it is told to or when the session closes. So:
 *
 * - the first window watched fills the screen (the primary);
 * - any other window watched while one is up gets a floating window of its own (an extra);
 * - asking for a window already showing, either way, does nothing;
 * - when the primary ends, the extras stay; they end when the user closes them, when the host
 *   closes their window, or with the session.
 *
 * Plain Kotlin on purpose, so the rules are tested without a device.
 */
class WatchSet {
    enum class Plan { PRIMARY, EXTRA, ALREADY_SHOWING }

    /** The window filling the screen, if any. */
    var primary: Long? = null
        private set

    private val extras = LinkedHashSet<Long>()

    /** What asking to watch [window] would do right now. */
    fun plan(window: Long): Plan = when {
        window == primary || window in extras -> Plan.ALREADY_SHOWING
        primary == null -> Plan.PRIMARY
        else -> Plan.EXTRA
    }

    /** Asks to watch [window]: records it as shown the way [plan] says, and returns that plan. */
    fun begin(window: Long): Plan = plan(window).also {
        when (it) {
            Plan.PRIMARY -> primary = window
            Plan.EXTRA -> extras += window
            Plan.ALREADY_SHOWING -> {}
        }
    }

    fun isExtra(window: Long): Boolean = window in extras

    fun isWatched(window: Long): Boolean = window == primary || window in extras

    /** The extras, oldest first. */
    fun extras(): List<Long> = extras.toList()

    /** The full-screen window ended. The extras are not touched. */
    fun primaryEnded() {
        primary = null
    }

    fun extraEnded(window: Long) {
        extras -= window
    }

    /** The session ended: nothing is showing any more. */
    fun clear() {
        primary = null
        extras.clear()
    }
}
