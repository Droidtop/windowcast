package dev.windowcast

import dev.windowcast.InputAdapter.Finger
import dev.windowcast.InputAdapter.Gesture
import dev.windowcast.InputAdapter.KeyResult
import dev.windowcast.InputAdapter.TouchPoint
import org.junit.Assert.assertEquals
import org.junit.Test

class InputAdapterTest {
    private val two = listOf(Finger(3, 100f, 50f), Finger(7, 200f, 150f))

    @Test
    fun aFingerGoingDownIsToldAloneAsShareOfTheView() {
        assertEquals(
            listOf(TouchPoint(7, 0.5f, 0.75f, Input.Touch.Start)),
            InputAdapter.touchPoints(Gesture.PointerDown, 1, two, 400, 200),
        )
    }

    @Test
    fun aMoveIsToldForEveryFinger() {
        val told = InputAdapter.touchPoints(Gesture.Move, 0, two, 400, 200)
        assertEquals(listOf(3, 7), told.map { it.id })
        assertEquals(setOf(Input.Touch.Move), told.map { it.phase }.toSet())
    }

    @Test
    fun aFingerLeavingIsToldAloneAndACancelForEveryFinger() {
        assertEquals(
            listOf(TouchPoint(3, 0.25f, 0.25f, Input.Touch.End)),
            InputAdapter.touchPoints(Gesture.PointerUp, 0, two, 400, 200),
        )
        assertEquals(2, InputAdapter.touchPoints(Gesture.Cancel, 0, two, 400, 200).size)
    }

    @Test
    fun otherGesturesAndMissingFingersTellNothing() {
        assertEquals(emptyList<TouchPoint>(), InputAdapter.touchPoints(Gesture.Other, 0, two, 400, 200))
        assertEquals(emptyList<TouchPoint>(), InputAdapter.touchPoints(Gesture.Down, 5, two, 400, 200))
    }

    @Test
    fun aViewWithNoSizeYetDoesNotDivideByZero() {
        val told = InputAdapter.touchPoints(Gesture.Down, 0, listOf(Finger(1, 10f, 20f)), 0, 0)
        assertEquals(10f, told.single().x, 0f)
        assertEquals(20f, told.single().y, 0f)
    }

    @Test
    fun aKeyWithACodeGoesAsThatKeyAndItsRepeatsAreSwallowed() {
        assertEquals(KeyResult.Key(30, true), InputAdapter.keyResult(30, true, 0, 'a'.code))
        assertEquals(KeyResult.Key(30, false), InputAdapter.keyResult(30, false, 0, 'a'.code))
        assertEquals(KeyResult.Swallow, InputAdapter.keyResult(30, true, 3, 'a'.code))
    }

    @Test
    fun anyOtherCharacterGoesAsTextOnItsPress() {
        assertEquals(KeyResult.Text("é"), InputAdapter.keyResult(null, true, 0, 'é'.code))
        assertEquals(KeyResult.Ignore, InputAdapter.keyResult(null, false, 0, 'é'.code))
        assertEquals(KeyResult.Ignore, InputAdapter.keyResult(null, true, 0, 0))
    }
}
