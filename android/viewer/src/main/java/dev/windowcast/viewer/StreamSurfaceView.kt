package dev.windowcast.viewer

import android.content.Context
import android.text.InputType
import android.view.KeyEvent
import android.view.SurfaceView
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection

/**
 * A stream's surface that can take typing. Android offers a soft keyboard only to a view that
 * says it is a text editor and hands the keyboard an [InputConnection]; without that, a device
 * with no hardware keyboard could not type into a window at all.
 *
 * Typing goes only where hardware typing already goes. Keys the keyboard sends as key events
 * reach the activity's key handling, which maps them to the host's keys. Text a keyboard can
 * only commit as characters (an emoji, a character its layout has no key for) goes to
 * [onText], which the viewer sends as text input. Nothing is held back or composed here, so
 * what is typed is what the host receives.
 */
class StreamSurfaceView(context: Context, private val onText: (String) -> Unit) : SurfaceView(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // No text field behind this, so no suggestions or composing: ask for plain key events.
        outAttrs.inputType = InputType.TYPE_NULL
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        val owner = this
        return object : BaseInputConnection(owner, false) {
            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                if (!text.isNullOrEmpty()) onText(text.toString())
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                // As backspace and delete presses, the way a hardware keyboard makes them. A
                // keyboard that asks for a huge deletion is not obeyed in full.
                repeat(beforeLength.coerceIn(0, MAX_DELETE)) { press(KeyEvent.KEYCODE_DEL) }
                repeat(afterLength.coerceIn(0, MAX_DELETE)) { press(KeyEvent.KEYCODE_FORWARD_DEL) }
                return true
            }

            private fun press(keyCode: Int) {
                sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
                sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, keyCode))
            }
        }
    }

    private companion object {
        const val MAX_DELETE = 64
    }
}
