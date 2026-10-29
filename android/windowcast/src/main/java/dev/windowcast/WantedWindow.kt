package dev.windowcast

/**
 * A window another app asked for by part of its title (docs/ANDROID-VIEWER.md, "Opening a host from
 * another app"): watched as soon as the host lists it, the list asked for again every [refreshMs]
 * until [withinMs] have passed (a game started just now takes a while to open a window).
 *
 * Plain Kotlin with the time passed in, so the rule is tested without a device, and so every client
 * of the library (the viewer, droidtop) waits for a window the same way.
 */
class WantedWindow(
    val title: String,
    private val startedAtMs: Long,
    private val withinMs: Long = WITHIN_MS,
    private val refreshMs: Long = REFRESH_MS,
) {
    sealed interface Step {
        /** The window to watch now. */
        data class Found(val window: WindowInfo) : Step

        /** Not listed yet: ask the host for its windows again in [retryInMs]. */
        data class Wait(val retryInMs: Long) : Step

        /** None appeared in time. */
        data object GiveUp : Step
    }

    /** What to do with [list], the windows the host just listed, at [nowMs]. A window the host cannot capture (0x0) never matches. */
    fun next(list: List<WindowInfo>, nowMs: Long): Step {
        val found = list.firstOrNull { it.width > 0 && it.height > 0 && it.title.contains(title, ignoreCase = true) }
        return when {
            found != null -> Step.Found(found)
            nowMs - startedAtMs > withinMs -> Step.GiveUp
            else -> Step.Wait(refreshMs)
        }
    }

    companion object {
        const val REFRESH_MS = 2_000L
        const val WITHIN_MS = 90_000L
    }
}
