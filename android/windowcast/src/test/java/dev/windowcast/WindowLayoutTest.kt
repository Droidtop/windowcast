package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class WindowLayoutTest {
    private fun info(id: Long, w: Int, h: Int, position: Pair<Int, Int>?, owner: Long? = null) =
        WindowInfo(id, "t", "app", w, h, focused = false, content = "general", owner = owner, kind = "popup", position = position)

    @Test
    fun aSmallWindowKeepsItsSize() {
        assertEquals(400 to 300, WindowLayout.floatingSize(400, 300, 1920, 1080))
    }

    @Test
    fun aLargeWindowShrinksToNinetyAndEightyPercentKeepingItsShape() {
        // 4000x2000 on 1000x1000: width allows 0.225, height allows 0.4; the smaller wins.
        assertEquals(900 to 450, WindowLayout.floatingSize(4000, 2000, 1000, 1000))
    }

    @Test
    fun anUnsizedWindowIsStillDrawnAtTheMinimum() {
        assertEquals(64 to 64, WindowLayout.floatingSize(0, 0, 1920, 1080))
        assertEquals(64 to 64, WindowLayout.floatingSize(10, 10, 1920, 1080))
    }

    @Test
    fun aPopupSitsWhereItIsAgainstItsOwnerScaledToHowTheOwnerIsShown() {
        val owner = info(1, 1000, 500, 100 to 200)
        val popup = info(2, 200, 100, 150 to 260, owner = 1)
        // The owner (1000x500 on the host) is shown at 500x250 with its top-left at (10, 20): half size.
        assertEquals(
            WindowLayout.Box(x = 10 + 25, y = 20 + 30, width = 100, height = 50),
            WindowLayout.againstOwner(popup, owner, 10, 20, 500, 250),
        )
    }

    @Test
    fun aTinyPopupIsStillDrawnAtTheMinimum() {
        val owner = info(1, 1000, 1000, 0 to 0)
        val popup = info(2, 10, 10, 5 to 5, owner = 1)
        val box = WindowLayout.againstOwner(popup, owner, 0, 0, 100, 100)!!
        assertEquals(32, box.width)
        assertEquals(32, box.height)
    }

    @Test
    fun withoutBothPositionsThereIsNothingToPlaceAgainst() {
        val owner = info(1, 1000, 500, null)
        val placed = info(2, 200, 100, 150 to 260, owner = 1)
        assertNull(WindowLayout.againstOwner(placed, owner, 0, 0, 500, 250))
        assertNull(WindowLayout.againstOwner(info(3, 10, 10, null, owner = 1), info(1, 1000, 500, 0 to 0), 0, 0, 500, 250))
    }
}
