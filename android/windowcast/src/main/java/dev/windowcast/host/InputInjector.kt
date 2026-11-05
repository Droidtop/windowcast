package dev.windowcast.host

import android.content.Context
import android.hardware.input.InputManager
import android.os.SystemClock
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import dev.windowcast.Keys
import org.json.JSONObject

/**
 * Puts a client's input into one app's display (Droidtop/tracker#615): pointer, wheel, touch,
 * keys and text, as the shell user may inject them. `InputManager.injectInputEvent` and
 * `InputEvent.setDisplayId` are hidden APIs that shell holds the permission for (scrcpy injects
 * the same way). Coordinates arrive as fractions of the window, which is the display. One per
 * app window, used on the host's command thread only.
 */
internal class InputInjector(context: Context, private val displayId: Int, private val width: Int, private val height: Int) {
    private val manager = context.getSystemService(InputManager::class.java)
    private var pointerX = 0f
    private var pointerY = 0f
    private var buttons = 0
    private var downAt = 0L

    /** Fingers down now, by the client's id, at their pixels. */
    private val fingers = linkedMapOf<Int, Pair<Float, Float>>()

    /** Injects the host's input event [json] (`{"PointerMove":{...}}`). Unknown kinds are ignored. */
    fun inject(json: String) {
        val outer = JSONObject(json)
        val kind = outer.keys().asSequence().firstOrNull() ?: return
        val e = outer.optJSONObject(kind) ?: return
        when (kind) {
            "PointerMove" -> {
                pointerX = e.getDouble("x").toFloat() * width
                pointerY = e.getDouble("y").toFloat() * height
                mouse(if (buttons == 0) MotionEvent.ACTION_HOVER_MOVE else MotionEvent.ACTION_MOVE, 0)
            }
            "PointerButton" -> {
                val button = when (e.getString("button")) {
                    "Left" -> MotionEvent.BUTTON_PRIMARY
                    "Right" -> MotionEvent.BUTTON_SECONDARY
                    "Middle" -> MotionEvent.BUTTON_TERTIARY
                    "Back" -> MotionEvent.BUTTON_BACK
                    "Forward" -> MotionEvent.BUTTON_FORWARD
                    else -> return
                }
                if (e.getBoolean("pressed")) {
                    val first = buttons == 0
                    buttons = buttons or button
                    if (first) {
                        downAt = SystemClock.uptimeMillis()
                        mouse(MotionEvent.ACTION_DOWN, 0)
                    }
                    mouse(MotionEvent.ACTION_BUTTON_PRESS, button)
                } else {
                    buttons = buttons and button.inv()
                    mouse(MotionEvent.ACTION_BUTTON_RELEASE, button)
                    if (buttons == 0) mouse(MotionEvent.ACTION_UP, 0)
                }
            }
            "PointerScroll" -> scroll(e.getDouble("dx").toFloat(), e.getDouble("dy").toFloat())
            "Touch" -> touch(e.getInt("id"), e.getDouble("x").toFloat() * width, e.getDouble("y").toFloat() * height, e.getString("phase"))
            "Key" -> {
                val code = Keys.android(e.getInt("keycode")) ?: return
                val now = SystemClock.uptimeMillis()
                val action = if (e.getBoolean("pressed")) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP
                send(KeyEvent(now, now, action, code, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD))
            }
            "Text" -> {
                // The keys that type it on the virtual keyboard's map; characters it cannot type are skipped.
                val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(e.getString("text").toCharArray())
                events?.forEach { send(it) }
            }
        }
    }

    private fun mouse(action: Int, actionButton: Int) {
        val now = SystemClock.uptimeMillis()
        val properties = arrayOf(MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_MOUSE })
        val coords = arrayOf(MotionEvent.PointerCoords().apply { x = pointerX; y = pointerY })
        val event = MotionEvent.obtain(
            if (downAt == 0L) now else downAt, now, action, 1, properties, coords, 0, buttons, 1f, 1f, 0, 0,
            InputDevice.SOURCE_MOUSE, 0,
        )
        if (actionButton != 0) event.actionButton = actionButton
        send(event)
        if (action == MotionEvent.ACTION_UP) downAt = 0L
    }

    private fun scroll(dx: Float, dy: Float) {
        val now = SystemClock.uptimeMillis()
        val properties = arrayOf(MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_MOUSE })
        val coords = arrayOf(
            MotionEvent.PointerCoords().apply {
                x = pointerX
                y = pointerY
                setAxisValue(MotionEvent.AXIS_HSCROLL, dx)
                setAxisValue(MotionEvent.AXIS_VSCROLL, dy)
            },
        )
        send(MotionEvent.obtain(now, now, MotionEvent.ACTION_SCROLL, 1, properties, coords, 0, buttons, 1f, 1f, 0, 0, InputDevice.SOURCE_MOUSE, 0))
    }

    private fun touch(id: Int, x: Float, y: Float, phase: String) {
        val now = SystemClock.uptimeMillis()
        val wasDown = fingers.containsKey(id)
        when (phase) {
            "Start", "Move" -> fingers[id] = x to y
        }
        val ids = fingers.keys.toList()
        val index = ids.indexOf(id).coerceAtLeast(0)
        val action = when (phase) {
            "Start" -> if (fingers.size == 1) MotionEvent.ACTION_DOWN else MotionEvent.ACTION_POINTER_DOWN or (index shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
            "Move" -> if (wasDown) MotionEvent.ACTION_MOVE else return
            "End" -> if (fingers.size <= 1) MotionEvent.ACTION_UP else MotionEvent.ACTION_POINTER_UP or (index shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
            "Cancel" -> MotionEvent.ACTION_CANCEL
            else -> return
        }
        if (fingers.isEmpty()) return
        if (action == MotionEvent.ACTION_DOWN) downAt = now
        val properties = ids.map { pointer -> MotionEvent.PointerProperties().apply { this.id = pointer; toolType = MotionEvent.TOOL_TYPE_FINGER } }.toTypedArray()
        val coords = ids.map { pointer ->
            val (px, py) = fingers.getValue(pointer)
            MotionEvent.PointerCoords().apply {
                this.x = px
                this.y = py
                pressure = 1f
                size = 1f
            }
        }.toTypedArray()
        send(MotionEvent.obtain(downAt, now, action, ids.size, properties, coords, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0))
        if (phase == "End" || phase == "Cancel") fingers.remove(id)
        if (phase == "Cancel") fingers.clear()
    }

    private fun send(event: InputEvent) {
        try {
            setDisplayId.invoke(event, displayId)
            injectInputEvent.invoke(manager, event, INJECT_ASYNC)
        } catch (e: ReflectiveOperationException) {
            android.util.Log.w("windowcast.AndroidHost", "input not injected into display $displayId", e)
        }
    }

    private companion object {
        /** `InputManager.INJECT_INPUT_EVENT_MODE_ASYNC`: the event goes, nobody waits for it. */
        const val INJECT_ASYNC = 0

        val injectInputEvent = InputManager::class.java.getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
        val setDisplayId = InputEvent::class.java.getMethod("setDisplayId", Int::class.javaPrimitiveType)
    }
}
