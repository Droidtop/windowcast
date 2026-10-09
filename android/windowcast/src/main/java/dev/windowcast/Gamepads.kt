package dev.windowcast

import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import kotlin.math.roundToInt

/**
 * Turns Android gamepad events into [GamepadState]s, one pad per device (up
 * to four), and sends each change through [send].
 */
class Gamepads(private val send: (Input) -> Unit) {
    private val pads = LinkedHashMap<Int, Int>() // device id to pad number
    private val states = HashMap<Int, GamepadState>()

    /** True when the key was a gamepad button and was handled. */
    fun onKey(event: KeyEvent): Boolean {
        if (!isGamepad(event.source)) return false
        val bit = buttonBit(event.keyCode) ?: return false
        val pad = padFor(event.deviceId) ?: return false
        val old = states[pad] ?: GamepadState()
        val buttons = if (event.action == KeyEvent.ACTION_DOWN) old.buttons or bit else old.buttons and bit.inv()
        update(pad, old.copy(buttons = buttons))
        return true
    }

    /** True when the motion was a gamepad's sticks, triggers or hat. */
    fun onMotion(event: MotionEvent): Boolean {
        if (!isGamepad(event.source) || event.action != MotionEvent.ACTION_MOVE) return false
        val pad = padFor(event.deviceId) ?: return false
        val old = states[pad] ?: GamepadState()
        fun stick(axis: Int, invert: Boolean = false): Int {
            val v = event.getAxisValue(axis) * if (invert) -1f else 1f
            return (v.coerceIn(-1f, 1f) * 32767f).roundToInt()
        }
        fun trigger(vararg axes: Int) = (axes.maxOf { event.getAxisValue(it) }.coerceIn(0f, 1f) * 255f).roundToInt()
        val hatX = event.getAxisValue(MotionEvent.AXIS_HAT_X)
        val hatY = event.getAxisValue(MotionEvent.AXIS_HAT_Y)
        var buttons = old.buttons and
            (GamepadState.DPAD_UP or GamepadState.DPAD_DOWN or GamepadState.DPAD_LEFT or GamepadState.DPAD_RIGHT).inv()
        if (hatX < -0.5f) buttons = buttons or GamepadState.DPAD_LEFT
        if (hatX > 0.5f) buttons = buttons or GamepadState.DPAD_RIGHT
        if (hatY < -0.5f) buttons = buttons or GamepadState.DPAD_UP
        if (hatY > 0.5f) buttons = buttons or GamepadState.DPAD_DOWN
        update(
            pad,
            GamepadState(
                buttons = buttons,
                // Android's Y axes point down; windowcast's point up.
                leftX = stick(MotionEvent.AXIS_X),
                leftY = stick(MotionEvent.AXIS_Y, invert = true),
                rightX = stick(MotionEvent.AXIS_Z),
                rightY = stick(MotionEvent.AXIS_RZ, invert = true),
                leftTrigger = trigger(MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE),
                rightTrigger = trigger(MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS),
            ),
        )
        return true
    }

    /** Tells the host every pad is gone (the stream ended). */
    fun releaseAll() {
        for (pad in pads.values) send(Input.gamepadGone(pad))
        pads.clear()
        states.clear()
    }

    private fun update(pad: Int, state: GamepadState) {
        if (states[pad] != state) {
            states[pad] = state
            send(Input.gamepad(pad, state))
        }
    }

    private fun padFor(deviceId: Int): Int? {
        pads[deviceId]?.let { return it }
        val free = (0..3).firstOrNull { it !in pads.values } ?: return null
        pads[deviceId] = free
        return free
    }

    private fun isGamepad(source: Int) =
        source and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD ||
            source and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK

    private fun buttonBit(keyCode: Int): Int? = when (keyCode) {
        KeyEvent.KEYCODE_BUTTON_A -> GamepadState.A
        KeyEvent.KEYCODE_BUTTON_B -> GamepadState.B
        KeyEvent.KEYCODE_BUTTON_X -> GamepadState.X
        KeyEvent.KEYCODE_BUTTON_Y -> GamepadState.Y
        KeyEvent.KEYCODE_BUTTON_L1 -> GamepadState.LEFT_SHOULDER
        KeyEvent.KEYCODE_BUTTON_R1 -> GamepadState.RIGHT_SHOULDER
        KeyEvent.KEYCODE_BUTTON_THUMBL -> GamepadState.LEFT_THUMB
        KeyEvent.KEYCODE_BUTTON_THUMBR -> GamepadState.RIGHT_THUMB
        KeyEvent.KEYCODE_BUTTON_START -> GamepadState.START
        KeyEvent.KEYCODE_BUTTON_SELECT -> GamepadState.BACK
        KeyEvent.KEYCODE_BUTTON_MODE -> GamepadState.GUIDE
        KeyEvent.KEYCODE_DPAD_UP -> GamepadState.DPAD_UP
        KeyEvent.KEYCODE_DPAD_DOWN -> GamepadState.DPAD_DOWN
        KeyEvent.KEYCODE_DPAD_LEFT -> GamepadState.DPAD_LEFT
        KeyEvent.KEYCODE_DPAD_RIGHT -> GamepadState.DPAD_RIGHT
        else -> null
    }
}
