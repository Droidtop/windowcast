package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Test

class WantedWindowTest {
    private fun window(id: Long, title: String, width: Int = 800, height: Int = 600) =
        WindowInfo(id, title, "app", width, height, focused = false, content = "")

    @Test
    fun aListedWindowIsFoundByPartOfItsTitleWithoutCase() {
        val wanted = WantedWindow("zelda", startedAtMs = 0)
        val zelda = window(2, "The Legend of ZELDA")
        assertEquals(WantedWindow.Step.Found(zelda), wanted.next(listOf(window(1, "Notepad"), zelda), nowMs = 100))
    }

    @Test
    fun aWindowTheHostCannotCaptureIsNotTakenForTheOneWanted() {
        val wanted = WantedWindow("zelda", startedAtMs = 0)
        assertEquals(
            WantedWindow.Step.Wait(WantedWindow.REFRESH_MS),
            wanted.next(listOf(window(1, "Zelda launcher", width = 0, height = 0)), nowMs = 100),
        )
    }

    @Test
    fun noWindowYetMeansAskAgainUntilTheTimeIsUp() {
        val wanted = WantedWindow("zelda", startedAtMs = 1_000, withinMs = 10_000, refreshMs = 500)
        assertEquals(WantedWindow.Step.Wait(500), wanted.next(emptyList(), nowMs = 5_000))
        assertEquals(WantedWindow.Step.Wait(500), wanted.next(emptyList(), nowMs = 11_000))
        assertEquals(WantedWindow.Step.GiveUp, wanted.next(emptyList(), nowMs = 11_001))
    }

    @Test
    fun aMatchOnTheLastMomentStillWins() {
        val wanted = WantedWindow("zelda", startedAtMs = 0, withinMs = 10)
        val zelda = window(3, "Zelda")
        assertEquals(WantedWindow.Step.Found(zelda), wanted.next(listOf(zelda), nowMs = 1_000_000))
    }

    @Test
    fun theFirstOfTwoMatchesIsTaken() {
        val wanted = WantedWindow("game", startedAtMs = 0)
        val first = window(1, "game one")
        assertEquals(WantedWindow.Step.Found(first), wanted.next(listOf(first, window(2, "game two")), nowMs = 0))
    }
}
