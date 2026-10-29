package dev.windowcast

import dev.windowcast.WatchSet.Plan
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class WatchSetTest {
    @Test
    fun theFirstWindowFillsTheScreenAndTheNextOnesFloat() {
        val set = WatchSet()
        assertEquals(Plan.PRIMARY, set.begin(1))
        assertEquals(Plan.EXTRA, set.begin(2))
        assertEquals(Plan.EXTRA, set.begin(3))
        assertEquals(1L, set.primary)
        assertEquals(listOf(2L, 3L), set.extras())
    }

    @Test
    fun askingForAWindowAlreadyShowingDoesNothing() {
        val set = WatchSet()
        set.begin(1)
        set.begin(2)
        assertEquals(Plan.ALREADY_SHOWING, set.begin(1))
        assertEquals(Plan.ALREADY_SHOWING, set.begin(2))
        assertEquals(1L, set.primary)
        assertEquals(listOf(2L), set.extras())
    }

    @Test
    fun theExtrasOutliveThePrimary() {
        val set = WatchSet()
        set.begin(1)
        set.begin(2)
        set.primaryEnded()
        assertNull(set.primary)
        assertEquals(listOf(2L), set.extras())
        assertTrue(set.isWatched(2))
        assertFalse(set.isWatched(1))
        // The next window asked for fills the screen again, and the extra is still no-op.
        assertEquals(Plan.ALREADY_SHOWING, set.begin(2))
        assertEquals(Plan.PRIMARY, set.begin(3))
    }

    @Test
    fun aClosedWindowCanBeWatchedAgain() {
        val set = WatchSet()
        set.begin(1)
        set.begin(2)
        set.extraEnded(2)
        assertFalse(set.isExtra(2))
        assertEquals(Plan.EXTRA, set.begin(2))
        set.primaryEnded()
        assertEquals(Plan.PRIMARY, set.begin(1))
    }

    @Test
    fun theSessionEndingClosesEverything() {
        val set = WatchSet()
        set.begin(1)
        set.begin(2)
        set.begin(3)
        set.clear()
        assertNull(set.primary)
        assertEquals(emptyList<Long>(), set.extras())
        assertEquals(Plan.PRIMARY, set.begin(2))
    }

    @Test
    fun endingAWindowThatWasNeverShownChangesNothing() {
        val set = WatchSet()
        set.begin(1)
        set.extraEnded(9)
        set.primaryEnded()
        set.primaryEnded()
        assertNull(set.primary)
        assertEquals(emptyList<Long>(), set.extras())
    }
}
