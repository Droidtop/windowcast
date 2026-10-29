package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SignInPlanTest {
    private fun provider(name: String) = OidcProvider(name, "https://$name.example", "{}")

    private fun options(trusted: Boolean, password: Boolean, vararg providers: String) =
        SignInOptions("hostid", "ab:cd", trusted, password, providers.map(::provider))

    @Test
    fun aHostNotTrustedYetMustBeConfirmedAndItsIdentityIsAccepted() {
        val plan = SignInPlan.of(options(trusted = false, password = true))
        assertTrue(plan.needsConfirmation)
        assertEquals("hostid", plan.acceptHost)
        assertTrue(plan.confirmationText.contains("ab:cd"))
    }

    @Test
    fun aTrustedHostIsNotAskedAboutAndNothingIsAccepted() {
        val plan = SignInPlan.of(options(trusted = true, password = true))
        assertFalse(plan.needsConfirmation)
        assertNull(plan.acceptHost)
    }

    @Test
    fun methodsAreThePasswordThenBrowserAndCodePerProviderInOrder() {
        val plan = SignInPlan.of(options(true, true, "Google", "Entra"))
        assertEquals(
            listOf(
                "User name and password",
                "Google in the browser",
                "Google with a code on another device",
                "Entra in the browser",
                "Entra with a code on another device",
            ),
            plan.methods.map { it.label },
        )
    }

    @Test
    fun withoutPasswordOnlyTheProvidersAreOffered() {
        val plan = SignInPlan.of(options(true, false, "Google"))
        assertEquals(2, plan.methods.size)
        assertTrue(plan.methods.none { it is SignInPlan.Method.Password })
    }

    @Test
    fun aHostWithNothingWeCanDoHasNoMethodsAndSaysSo() {
        val plan = SignInPlan.of(options(true, false))
        assertTrue(plan.methods.isEmpty())
        assertEquals("This host takes no sign-in this app can do", plan.noMethodsLine)
    }

    @Test
    fun theFailureLineNamesTheProviderAndTheReason() {
        assertEquals("Could not sign in with Google: timed out", SignInPlan.failedWith("Google", "timed out"))
    }

    @Test
    fun aCancelledAttemptIsNotCurrentWhenItAnswers() {
        val attempts = SignInAttempts()
        val first = attempts.begin()
        assertTrue(attempts.waiting)
        assertTrue(attempts.isCurrent(first))
        attempts.cancel()
        assertFalse(attempts.waiting)
        assertFalse(attempts.isCurrent(first))
    }

    @Test
    fun aNewerAttemptReplacesAnOlderOne() {
        val attempts = SignInAttempts()
        val first = attempts.begin()
        val second = attempts.begin()
        assertFalse(attempts.isCurrent(first))
        assertTrue(attempts.isCurrent(second))
        attempts.finish()
        assertFalse(attempts.waiting)
    }
}
