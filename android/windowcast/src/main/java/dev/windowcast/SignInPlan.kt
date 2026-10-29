package dev.windowcast

/**
 * What a client offers a person who is signing in to a host with an account (docs/ACCOUNTS.md),
 * worked out from what the host said it takes ([SignInOptions]). The rules are here so every client
 * of the library asks the same questions in the same order and says the same things; the dialogs that
 * ask them are the client's.
 *
 * 1. A host this client does not trust yet must first be confirmed by its fingerprint
 *    ([needsConfirmation]); nothing is sent to it until the person says it matches, and then
 *    [acceptHost] is passed to [WindowcastClient.connectAccount].
 * 2. Then the person picks one of [methods]: a password, and for each of the host's providers the
 *    browser and a code on another device. A host that takes none of these has [methods] empty
 *    and [noMethodsLine] says so.
 *
 * Plain Kotlin, so it is tested without a device.
 */
class SignInPlan private constructor(
    val fingerprint: String,
    val needsConfirmation: Boolean,
    private val hostId: String,
    val methods: List<Method>,
) {
    sealed interface Method {
        /** The line in the list the person picks from. */
        val label: String

        data object Password : Method {
            override val label = "User name and password"
        }

        data class Browser(val provider: OidcProvider) : Method {
            override val label = "${provider.name} in the browser"
        }

        data class DeviceCode(val provider: OidcProvider) : Method {
            override val label = "${provider.name} with a code on another device"
        }
    }

    /** The host identity to pass as `acceptHost` once the person confirmed the fingerprint; null for a host already trusted. */
    val acceptHost: String? get() = hostId.takeIf { needsConfirmation }

    /** What to say when [methods] is empty. */
    val noMethodsLine = "This host takes no sign-in this app can do"

    /** The question shown with [fingerprint] to a person confirming a host they have not signed in to before. */
    val confirmationText: String
        get() = "This device has not signed in to this host before. Check that the host shows this " +
            "fingerprint:\n\n$fingerprint\n\nIf it shows another, cancel: a different machine is answering."

    companion object {
        const val CANCELLED = "Sign-in cancelled"

        fun of(options: SignInOptions): SignInPlan = SignInPlan(
            fingerprint = options.fingerprint,
            needsConfirmation = !options.trusted,
            hostId = options.hostId,
            methods = buildList {
                if (options.password) add(Method.Password)
                for (provider in options.providers) {
                    add(Method.Browser(provider))
                    add(Method.DeviceCode(provider))
                }
            },
        )

        /** The status line for a failed sign-in with [provider], from the failure's own [reason]. */
        fun failedWith(providerName: String, reason: String?): String = "Could not sign in with $providerName: $reason"
    }
}

/**
 * Which browser or device-code sign-in is still wanted. A sign-in waits on a provider for minutes; the
 * person may cancel, or start another, meanwhile, and when the first one finally answers its token must
 * be dropped, not used. [begin] numbers an attempt, [cancel] makes every earlier number stale, and the
 * attempt that answers asks [isCurrent] before using what it got (Droidtop/tracker#454).
 *
 * Thread-safe: attempts answer on threads of their own.
 */
class SignInAttempts {
    private val counter = java.util.concurrent.atomic.AtomicInteger()

    @Volatile
    var waiting = false
        private set

    /** A new attempt starts waiting. Returns its number. */
    fun begin(): Int {
        waiting = true
        return counter.incrementAndGet()
    }

    /** The attempt that answered, if it is still the one wanted. */
    fun isCurrent(attempt: Int): Boolean = counter.get() == attempt

    /** The attempt finished normally; no more waiting. */
    fun finish() {
        waiting = false
    }

    /** Gives up whatever is waiting: its answer will be ignored. */
    fun cancel() {
        counter.incrementAndGet()
        waiting = false
    }
}
