package dev.windowcast.viewer

/**
 * What starting a program on the host said ("Started notepad.exe (process 4242)"), kept in front of
 * the window-count line for [keepMs] (Droidtop/tracker#466).
 *
 * The list is asked for again a few times after a launch (the program's window takes a while to open),
 * and each answer rewrites the status line. A note shown once and then dropped was overwritten by the
 * next answer within a second, so it was never readable on a real host; it now stays through those
 * refreshes and ends by itself. Plain Kotlin with the time passed in, so it is tested off the device.
 */
class LaunchNote(private val keepMs: Long = KEEP_MS) {
    private var text: String? = null
    private var until = 0L

    fun set(note: String, nowMs: Long) {
        text = note
        until = nowMs + keepMs
    }

    /** The note while it is current, else null. */
    fun current(nowMs: Long): String? = text?.takeIf { nowMs < until }

    /** The status line for a window list of [count], with the note in front of it while it is current. */
    fun windowsLine(count: Int, nowMs: Long): String =
        (current(nowMs)?.let { "$it. " } ?: "") + "$count windows. Tap one to watch it."

    companion object {
        /** Long enough to be read, short enough to be gone when the person moves on. */
        const val KEEP_MS = 20_000L
    }
}
