package dev.windowcast.viewer

/**
 * What the viewer tells the user when something fails, in words that name the cause.
 * Plain Kotlin so the wording is tested without a device.
 */
object FailureText {
    /**
     * The library's own words (transport's NotPinned error: "the peer's identity is not pinned;
     * pair with it first") for a host this device has not paired with. The library gives the
     * viewer a message, not a kind, so this is the one thing matched on.
     */
    private const val NOT_PAIRED = "not pinned"

    /**
     * Why opening [address] from another app's link failed. A link never pairs, so a host this
     * device has not paired with is told so and pointed at the connect form; any other reason
     * (the host is off, the network, another computer answering) is given as it is, without the
     * pairing explanation, which used to be shown for every failure.
     */
    fun linkOpen(address: String, reason: String?): String {
        val why = reason?.trim().orEmpty()
        return when {
            why.contains(NOT_PAIRED, ignoreCase = true) ->
                "Could not open $address: this viewer has not paired with that host. " +
                    "Pair with it from the connect form first; a link never pairs."
            why.isEmpty() -> "Could not open $address: no reason was given"
            else -> "Could not open $address: $why"
        }
    }
}
