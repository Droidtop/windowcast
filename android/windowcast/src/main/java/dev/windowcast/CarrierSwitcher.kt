package dev.windowcast

/**
 * The state of changing a watched window from one carrier to another (docs/BACKENDS.md, "One window,
 * any carrier"): the switch under way, and the switches a host has refused, so a client stops offering
 * one the host has already said it cannot serve (Droidtop/tracker#471).
 *
 * The rules are here so every client of the library switches the same way; what is drawn is the
 * client's. [R] is whatever the client keeps for the new carrier while it waits for its first picture
 * (the viewer keeps the renderer). A client calls [begin] when the host starts the new carrier
 * ([Event.CarrierStarted]), [finish] on its first picture, [cancel] if the switch will not happen, and
 * [refuse] when the host refuses ([Event.CarrierRefused]).
 *
 * Plain Kotlin, called from one thread (the client's UI thread).
 */
class CarrierSwitcher<R : Any>(private val hostId: () -> String?) {
    /** A switch under way: the new carrier's generation and backend, and the client's thing for it. */
    class Pending<R : Any>(val generation: Int, val backend: String) {
        var renderer: R? = null
    }

    /** The switch under way, if any. */
    var pending: Pending<R>? = null
        private set

    /** Refusals as host:window:backend: per window, since a host can refuse one window's switch and serve another's. */
    private val refused = mutableSetOf<String>()

    private fun key(window: Long, backend: String) = "${hostId()}:$window:$backend"

    /** Whether a switch of [window] to [backend] is worth offering: false once this host refused it. A null window offers everything. */
    fun offered(window: Long?, backend: String): Boolean = window == null || key(window, backend) !in refused

    /** The host started [backend] for generation [generation]. Replaces any switch under way; the client cancels that one first. */
    fun begin(generation: Int, backend: String): Pending<R> = Pending<R>(generation, backend).also { pending = it }

    /** [switch] showed its first picture. True once, if it is still the switch under way; then it is no longer pending. */
    fun finish(switch: Pending<R>): Boolean {
        if (pending !== switch) return false
        pending = null
        return true
    }

    /** The switch under way will not happen. Returns it so the client can stop what it started, or null if none was. */
    fun cancel(): Pending<R>? = pending.also { pending = null }

    /** Whether the host's refusal of [generation] is for the switch under way (an older one's answer arriving late is not). */
    fun isPending(generation: Int): Boolean = pending?.generation == generation

    /** The host refused [backend] for [window]: [offered] says no from now on. */
    fun refuse(window: Long, backend: String) {
        refused += key(window, backend)
    }
}
