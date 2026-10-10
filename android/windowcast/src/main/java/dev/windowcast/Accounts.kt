package dev.windowcast

import java.io.Closeable
import java.io.IOException
import org.json.JSONObject

/** An OpenID Connect provider a host takes sign-ins from. */
class OidcProvider internal constructor(
    val name: String,
    val issuer: String,
    /** As client-core lists it, to hand back to it. */
    internal val json: String,
)

/**
 * What a host takes for account sign-in (docs/ACCOUNTS.md). Until [trusted], the user must
 * compare [fingerprint] with the one the host shows before anything is sent to it; the
 * providers are only the host's claim until then.
 */
class SignInOptions internal constructor(
    /** The host's identity (hex), for [WindowcastClient.connectAccount]'s acceptHost. */
    val hostId: String,
    val fingerprint: String,
    val trusted: Boolean,
    val password: Boolean,
    val providers: List<OidcProvider>,
) {
    internal companion object {
        fun parse(json: String): SignInOptions {
            val o = JSONObject(json)
            val methods = o.getJSONArray("methods")
            val providers = o.getJSONArray("providers")
            return SignInOptions(
                hostId = o.getString("host_id"),
                fingerprint = o.getString("fingerprint"),
                trusted = o.getBoolean("trusted"),
                password = (0 until methods.length()).any { methods.getString(it) == "Password" },
                providers = (0 until providers.length()).map { i ->
                    val p = providers.getJSONObject(i)
                    OidcProvider(p.getString("name"), p.getString("issuer"), p.toString())
                },
            )
        }
    }
}

/** How to sign in: a user name and password, or an ID token from one of the host's providers. */
sealed class SignIn {
    internal abstract val json: String

    class Password(username: String, password: String) : SignIn() {
        override val json: String = JSONObject().put(
            "password", JSONObject().put("username", username).put("password", password),
        ).toString()
    }

    class Oidc(provider: OidcProvider, idToken: String) : SignIn() {
        override val json: String = JSONObject().put(
            "oidc", JSONObject().put("provider", provider.name).put("id_token", idToken),
        ).toString()
    }
}

/**
 * A sign-in in the browser: open [url] (Custom Tabs), then [finish] waits for the provider to
 * send the browser back to the library's loopback port on this device.
 */
class OidcBrowserSignIn internal constructor(private var handle: Long, val url: String) {
    /** The ID token, for [SignIn.Oidc]. Blocks up to [timeoutMs]; call it once, off the main thread. */
    fun finish(timeoutMs: Int): String {
        val h = handle
        check(h != 0L) { "this sign-in is finished" }
        handle = 0L
        return Native.oidcBrowserFinish(h, timeoutMs) ?: throw IOException(Native.lastError())
    }
}

/**
 * A sign-in finished on another device: show [userCode] and [verificationUri] (or open
 * [verificationUriComplete], which has the code in it), then [await] until the user is done.
 * [close] gives up.
 */
class OidcDeviceSignIn internal constructor(
    private var handle: Long,
    val userCode: String,
    val verificationUri: String,
    val verificationUriComplete: String?,
) : Closeable {
    /**
     * Waits up to [timeoutMs]: the ID token once the user has signed in, null if not yet (call
     * again). Throws if they refused or the code expired. Blocks: call it off the main thread;
     * [close] waits for it.
     */
    @Synchronized
    fun await(timeoutMs: Int): String? {
        check(handle != 0L) { "this sign-in is closed" }
        val status = LongArray(1)
        val token = Native.oidcDeviceWait(handle, timeoutMs, status)
        if (token != null) return token
        if (status[0] == Native.TIMEOUT) return null
        throw IOException(Native.lastError())
    }

    @Synchronized
    override fun close() {
        if (handle != 0L) Native.oidcDeviceFree(handle)
        handle = 0L
    }

    internal companion object {
        fun parse(handle: Long, json: String): OidcDeviceSignIn {
            val o = JSONObject(json)
            return OidcDeviceSignIn(
                handle,
                o.getString("user_code"),
                o.getString("verification_uri"),
                if (o.isNull("verification_uri_complete")) null else o.getString("verification_uri_complete"),
            )
        }
    }
}
