package dev.windowcast

import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View

/**
 * Android input as the host's [Input]: fingers on a streamed window's view, and keys. The viewer and
 * droidtop send the same input for the same events. The rules are plain Kotlin over numbers, tested
 * without a device; the functions that take events only read them.
 */
object InputAdapter {
    /** One finger as the host is told about it: where in the view (0..1 each way) and what it did. */
    data class TouchPoint(val id: Int, val x: Float, val y: Float, val phase: Input.Touch)

    /** What a key press comes to. */
    sealed interface KeyResult {
        /** Send this key to the host. */
        data class Key(val evdev: Int, val pressed: Boolean) : KeyResult

        /** Send this character as text. */
        data class Text(val text: String) : KeyResult

        /** Handled, nothing to send (a held key's repeats). */
        data object Swallow : KeyResult

        /** Not one to send; leave it to the caller. */
        data object Ignore : KeyResult
    }

    /** A finger's position in the view's own pixels. */
    data class Finger(val id: Int, val x: Float, val y: Float)

    /** The part of a touch event the rules read. */
    enum class Gesture { Down, PointerDown, Move, PointerUp, Up, Cancel, Other }

    /**
     * What the host is told for a touch event: a finger going down or up is told on its own, a move
     * on every finger, a cancel on every finger. [fingers] are in the view's pixels; positions go out
     * as a share of [viewWidth] and [viewHeight] (at least one pixel each).
     */
    fun touchPoints(
        gesture: Gesture,
        actionIndex: Int,
        fingers: List<Finger>,
        viewWidth: Int,
        viewHeight: Int,
    ): List<TouchPoint> {
        fun point(f: Finger, phase: Input.Touch) =
            TouchPoint(f.id, f.x / viewWidth.coerceAtLeast(1), f.y / viewHeight.coerceAtLeast(1), phase)
        return when (gesture) {
            Gesture.Down, Gesture.PointerDown -> listOfNotNull(fingers.getOrNull(actionIndex)?.let { point(it, Input.Touch.Start) })
            Gesture.Move -> fingers.map { point(it, Input.Touch.Move) }
            Gesture.Up, Gesture.PointerUp -> listOfNotNull(fingers.getOrNull(actionIndex)?.let { point(it, Input.Touch.End) })
            Gesture.Cancel -> fingers.map { point(it, Input.Touch.Cancel) }
            Gesture.Other -> emptyList()
        }
    }

    /**
     * What a key comes to: a key with an evdev code goes as that key (a repeat is swallowed, the host
     * repeats it itself), any other character as text on its press, anything else is not ours.
     */
    fun keyResult(evdev: Int?, pressed: Boolean, repeatCount: Int, unicodeChar: Int): KeyResult = when {
        evdev != null -> if (repeatCount == 0) KeyResult.Key(evdev, pressed) else KeyResult.Swallow
        pressed && unicodeChar != 0 -> KeyResult.Text(String(Character.toChars(unicodeChar)))
        else -> KeyResult.Ignore
    }

    /** The input for every finger of [event] on [view], as touches on [window]. */
    fun touches(window: Long, view: View, event: MotionEvent): List<Input> {
        val gesture = when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> Gesture.Down
            MotionEvent.ACTION_POINTER_DOWN -> Gesture.PointerDown
            MotionEvent.ACTION_MOVE -> Gesture.Move
            MotionEvent.ACTION_POINTER_UP -> Gesture.PointerUp
            MotionEvent.ACTION_UP -> Gesture.Up
            MotionEvent.ACTION_CANCEL -> Gesture.Cancel
            else -> Gesture.Other
        }
        val fingers = (0 until event.pointerCount).map { Finger(event.getPointerId(it), event.getX(it), event.getY(it)) }
        return touchPoints(gesture, event.actionIndex, fingers, view.width, view.height)
            .map { Input.touch(window, it.id, it.x, it.y, it.phase) }
    }

    /**
     * The input for a key [event], and whether it is handled: [KeyResult.Ignore] gives null input and
     * false, the caller deals with the key itself. Gamepad buttons are [Gamepads]', so they are not
     * handled here.
     */
    fun key(event: KeyEvent): Pair<Input?, Boolean> =
        when (val r = keyResult(Keys.evdev(event.keyCode), event.action == KeyEvent.ACTION_DOWN, event.repeatCount, event.unicodeChar)) {
            is KeyResult.Key -> Input.key(r.evdev, r.pressed) to true
            is KeyResult.Text -> Input.text(r.text) to true
            KeyResult.Swallow -> null to true
            KeyResult.Ignore -> null to false
        }
}
