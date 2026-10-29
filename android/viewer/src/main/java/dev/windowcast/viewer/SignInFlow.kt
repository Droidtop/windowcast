package dev.windowcast.viewer

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.text.InputType
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import androidx.browser.customtabs.CustomTabsIntent
import dev.windowcast.OidcDeviceSignIn
import dev.windowcast.OidcProvider
import dev.windowcast.SignIn
import dev.windowcast.SignInAttempts
import dev.windowcast.SignInOptions
import dev.windowcast.SignInPlan
import dev.windowcast.WindowcastClient
import java.io.IOException
import java.util.concurrent.Executor
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The viewer's account sign-in (docs/ACCOUNTS.md): asking the host what it takes, confirming a
 * host not trusted yet, and the password, browser and device-code ways in. It owns no session:
 * a sign-in that succeeds hands over to [connectAccount], which starts it.
 *
 * Library calls block, so they run on [worker] (short asks) or [signInWorker] (waits for a
 * person); results come back to the main thread through [main].
 */
class SignInFlow(
    private val activity: Activity,
    private val main: Handler,
    private val status: TextView,
    private val cancelBrowser: View,
    private val client: () -> WindowcastClient?,
    private val worker: Executor,
    private val signInWorker: Executor,
    private val connectAccount: (address: String, signIn: SignIn, accept: String?) -> Unit,
) {
    private val attempts = SignInAttempts()

    /** Gives up the browser sign-in that is waiting, if any; its result is ignored when it arrives or times out. */
    fun giveUpBrowserSignIn(message: String) {
        attempts.cancel()
        cancelBrowser.visibility = View.GONE
        status.text = message
    }

    /**
     * Account sign-in (docs/ACCOUNTS.md): ask the host what it takes; a host not trusted yet
     * shows its fingerprint first, and nothing is sent to it until the user says it matches.
     */
    fun signIn(address: String) {
        val c = client() ?: return
        if (address.isEmpty()) {
            status.text = "Type the host's address first"
            return
        }
        status.text = "Asking $address how to sign in…"
        worker.execute {
            try {
                val options = c.signInOptions(address)
                main.post { confirmHost(address, options) }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "ask the host how to sign in failed", e)
                main.post { status.text = "Could not ask the host: ${e.message}" }
            }
        }
    }

    private fun confirmHost(address: String, options: SignInOptions) {
        val plan = SignInPlan.of(options)
        if (!plan.needsConfirmation) {
            chooseSignIn(address, plan)
            return
        }
        AlertDialog.Builder(activity)
            .setTitle("Is this your host?")
            .setMessage(plan.confirmationText)
            .setPositiveButton("It matches") { _, _ -> chooseSignIn(address, plan) }
            .setNegativeButton("Cancel") { _, _ -> status.text = SignInPlan.CANCELLED }
            .show()
    }

    /** The ways this host takes ([SignInPlan.methods]); [SignInPlan.acceptHost] is the identity the person confirmed, if any. */
    private fun chooseSignIn(address: String, plan: SignInPlan) {
        if (plan.methods.isEmpty()) {
            status.text = plan.noMethodsLine
            return
        }
        AlertDialog.Builder(activity)
            .setTitle("Sign in")
            .setItems(plan.methods.map { it.label }.toTypedArray()) { _, which ->
                when (val method = plan.methods[which]) {
                    SignInPlan.Method.Password -> askPassword(address, plan.acceptHost)
                    is SignInPlan.Method.Browser -> browserSignIn(address, method.provider, plan.acceptHost)
                    is SignInPlan.Method.DeviceCode -> deviceSignIn(address, method.provider, plan.acceptHost)
                }
            }
            .setNegativeButton("Cancel") { _, _ -> status.text = SignInPlan.CANCELLED }
            .show()
    }

    private fun askPassword(address: String, accept: String?) {
        val user = EditText(activity).apply { hint = "User name"; inputType = InputType.TYPE_CLASS_TEXT }
        val password = EditText(activity).apply {
            hint = "Password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        val fields = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 16, 48, 0)
            addView(user, MATCH_PARENT, WRAP_CONTENT)
            addView(password, MATCH_PARENT, WRAP_CONTENT)
        }
        AlertDialog.Builder(activity)
            .setTitle("Sign in with a password")
            .setView(fields)
            .setPositiveButton("Sign in") { _, _ ->
                connectAccount(address, SignIn.Password(user.text.toString().trim(), password.text.toString()), accept)
            }
            .setNegativeButton("Cancel") { _, _ -> status.text = SignInPlan.CANCELLED }
            .show()
    }

    /**
     * The provider's page in a Custom Tab. The provider sends the browser back to the library's
     * loopback port on this device, which hands over the ID token; then this activity comes back
     * to the front, closing the tab.
     */
    private fun browserSignIn(address: String, provider: OidcProvider, accept: String?) {
        val c = client() ?: return
        val attempt = attempts.begin()
        cancelBrowser.visibility = View.VISIBLE
        status.text = "Opening ${provider.name}…"
        signInWorker.execute {
            try {
                val signIn = c.oidcBrowser(provider)
                main.post {
                    try {
                        CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(signIn.url))
                    } catch (e: android.content.ActivityNotFoundException) {
                        status.text = "No browser to sign in with: ${e.message}"
                    }
                }
                val token = signIn.finish(SIGN_IN_TIMEOUT_MS)
                main.post {
                    // Cancelled meanwhile: the token is dropped.
                    if (!attempts.isCurrent(attempt)) return@post
                    attempts.finish()
                    cancelBrowser.visibility = View.GONE
                    backToFront()
                    connectAccount(address, SignIn.Oidc(provider, token), accept)
                }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "browser sign-in failed", e)
                main.post {
                    if (!attempts.isCurrent(attempt)) return@post
                    giveUpBrowserSignIn(SignInPlan.failedWith(provider.name, e.message))
                }
            }
        }
    }

    /** The provider's code, entered on another device (or opened here); waits until the user is done. */
    private fun deviceSignIn(address: String, provider: OidcProvider, accept: String?) {
        val c = client() ?: return
        status.text = "Asking ${provider.name} for a code…"
        signInWorker.execute {
            val signIn = try {
                c.oidcDevice(provider)
            } catch (e: IOException) {
                android.util.Log.w(TAG, "device sign-in failed", e)
                main.post { status.text = SignInPlan.failedWith(provider.name, e.message) }
                null
            }
            if (signIn != null) awaitDeviceSignIn(address, provider, accept, signIn)
        }
    }

    /** Shows the code and waits (on the calling thread) until the user is done or cancels. */
    private fun awaitDeviceSignIn(address: String, provider: OidcProvider, accept: String?, signIn: OidcDeviceSignIn) {
        val cancelled = AtomicBoolean(false)
        val dialog = arrayOfNulls<AlertDialog>(1)
        main.post {
            dialog[0] = AlertDialog.Builder(activity)
                .setTitle("Sign in with ${provider.name}")
                .setMessage("On another device, open\n\n${signIn.verificationUri}\n\nand enter the code\n\n${signIn.userCode}")
                .setNegativeButton("Cancel") { _, _ -> cancelled.set(true) }
                .setNeutralButton("Open it here") { _, _ ->
                    // The dialog closes; the wait goes on until the provider says.
                    val page = signIn.verificationUriComplete ?: signIn.verificationUri
                    CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(page))
                }
                .setCancelable(false)
                .show()
            status.text = "Waiting for the sign-in with ${provider.name}…"
        }
        try {
            var waited: String? = null
            while (waited == null && !cancelled.get()) waited = signIn.await(1000)
            val token = waited
            main.post {
                dialog[0]?.dismiss()
                if (token != null) {
                    backToFront()
                    connectAccount(address, SignIn.Oidc(provider, token), accept)
                } else {
                    status.text = SignInPlan.CANCELLED
                }
            }
        } catch (e: IOException) {
            android.util.Log.w(TAG, "device sign-in failed", e)
            main.post {
                dialog[0]?.dismiss()
                status.text = SignInPlan.failedWith(provider.name, e.message)
            }
        } finally {
            signIn.close()
        }
    }

    /** Back from the browser: if the sign-in there did not finish, say what is happening and how to stop it. */
    fun resumed() {
        if (attempts.waiting) {
            status.text = "Still waiting for the sign-in in the browser. Finish it there, or press Cancel sign-in."
        }
    }

    /** Brings this activity back over a Custom Tab opened from it, closing the tab. */
    private fun backToFront() {
        activity.startActivity(
            Intent(activity, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        )
    }

    private companion object {
        const val TAG = "windowcast"

        /** How long a browser sign-in may take before it is given up. */
        const val SIGN_IN_TIMEOUT_MS = 2 * 60 * 1000
    }
}
