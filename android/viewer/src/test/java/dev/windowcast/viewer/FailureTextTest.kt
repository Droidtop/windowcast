package dev.windowcast.viewer

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class FailureTextTest {
    @Test
    fun anUnpairedHostIsToldSoAndPointedAtTheConnectForm() {
        val text = FailureText.linkOpen("pc:47100", "the peer's identity is not pinned; pair with it first")
        assertTrue(text, text.contains("has not paired"))
        assertTrue(text, text.contains("connect form"))
    }

    @Test
    fun anyOtherReasonIsGivenAsItIsWithoutThePairingExplanation() {
        val text = FailureText.linkOpen("pc:47100", "timed out")
        assertEquals("Could not open pc:47100: timed out", text)
        assertFalse(text.contains("paired"))
        val other = FailureText.linkOpen(
            "pc:47100",
            "it answered as another computer (0123456789abcdef…), not the one asked for",
        )
        assertFalse(other.contains("has not paired"))
        assertTrue(other.contains("another computer"))
    }

    @Test
    fun aMissingReasonSaysSoInsteadOfNull() {
        assertEquals("Could not open pc:47100: no reason was given", FailureText.linkOpen("pc:47100", null))
        assertEquals("Could not open pc:47100: no reason was given", FailureText.linkOpen("pc:47100", "  "))
    }

    @Test
    fun theMatchIgnoresCase() {
        assertTrue(FailureText.linkOpen("a:1", "Peer Not Pinned").contains("has not paired"))
    }
}
