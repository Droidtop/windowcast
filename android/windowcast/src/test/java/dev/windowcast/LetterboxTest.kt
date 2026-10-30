package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class LetterboxTest {
    @Test
    fun aPictureOfTheBoxsOwnShapeFillsIt() {
        assertEquals(1920 to 1080, Letterbox.fit(1280, 720, 1920, 1080))
        assertEquals(1920 to 1080, Letterbox.fit(1920, 1080, 1920, 1080))
    }

    @Test
    fun aNarrowerPictureGetsBarsAtTheSides() {
        // 4:3 in 16:9: the height is used in full.
        assertEquals(1440 to 1080, Letterbox.fit(800, 600, 1920, 1080))
        // Taller than wide.
        assertEquals(607 to 1080, Letterbox.fit(1080, 1920, 1920, 1080))
    }

    @Test
    fun aWiderPictureGetsBarsAboveAndBelow() {
        // 21:9 in 16:9: the width is used in full.
        assertEquals(1920 to 822, Letterbox.fit(2560, 1097, 1920, 1080))
        assertEquals(1000 to 250, Letterbox.fit(400, 100, 1000, 1000))
    }

    @Test
    fun theShapeIsKeptAndTheBoxIsNeverExceeded() {
        for ((pw, ph) in listOf(1 to 1, 3 to 7, 640 to 480, 1920 to 1080, 5000 to 20, 20 to 5000)) {
            for ((bw, bh) in listOf(1080 to 1920, 1920 to 1080, 800 to 480, 17 to 13)) {
                val (w, h) = Letterbox.fit(pw, ph, bw, bh)!!
                assertTrue("$pw x $ph in $bw x $bh gave $w x $h", w in 1..bw && h in 1..bh)
                // Integer rounding aside, the shape is the picture's: one side fills the box.
                assertTrue(w == bw || h == bh)
            }
        }
    }

    @Test
    fun nothingToFitYetIsNull() {
        assertNull(Letterbox.fit(0, 600, 1920, 1080))
        assertNull(Letterbox.fit(800, 0, 1920, 1080))
        assertNull(Letterbox.fit(800, 600, 0, 1080))
        assertNull(Letterbox.fit(800, 600, 1920, -1))
    }

    @Test
    fun hugeSizesDoNotOverflow() {
        val (w, h) = Letterbox.fit(Int.MAX_VALUE, 1, Int.MAX_VALUE, Int.MAX_VALUE)!!
        assertEquals(Int.MAX_VALUE, w)
        assertEquals(1, h)
    }
}
