package dev.windowcast

import android.view.KeyEvent

/** Android key codes to the evdev key codes windowcast sends. */
object Keys {
    private val map: Map<Int, Int> = buildMap {
        // Letters, in evdev's QWERTY-ordered numbering.
        val letters = "qwertyuiop" to 16
        val home = "asdfghjkl" to 30
        val bottom = "zxcvbnm" to 44
        for ((row, start) in listOf(letters, home, bottom)) {
            row.forEachIndexed { i, c -> put(KeyEvent.KEYCODE_A + (c - 'a'), start + i) }
        }
        // 1-9 then 0.
        for (d in 1..9) put(KeyEvent.KEYCODE_0 + d, 1 + d)
        put(KeyEvent.KEYCODE_0, 11)
        put(KeyEvent.KEYCODE_ESCAPE, 1)
        put(KeyEvent.KEYCODE_MINUS, 12)
        put(KeyEvent.KEYCODE_EQUALS, 13)
        put(KeyEvent.KEYCODE_DEL, 14)
        put(KeyEvent.KEYCODE_TAB, 15)
        put(KeyEvent.KEYCODE_LEFT_BRACKET, 26)
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, 27)
        put(KeyEvent.KEYCODE_ENTER, 28)
        put(KeyEvent.KEYCODE_CTRL_LEFT, 29)
        put(KeyEvent.KEYCODE_SEMICOLON, 39)
        put(KeyEvent.KEYCODE_APOSTROPHE, 40)
        put(KeyEvent.KEYCODE_GRAVE, 41)
        put(KeyEvent.KEYCODE_SHIFT_LEFT, 42)
        put(KeyEvent.KEYCODE_BACKSLASH, 43)
        put(KeyEvent.KEYCODE_COMMA, 51)
        put(KeyEvent.KEYCODE_PERIOD, 52)
        put(KeyEvent.KEYCODE_SLASH, 53)
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, 54)
        put(KeyEvent.KEYCODE_ALT_LEFT, 56)
        put(KeyEvent.KEYCODE_SPACE, 57)
        put(KeyEvent.KEYCODE_CAPS_LOCK, 58)
        for (f in 0..9) put(KeyEvent.KEYCODE_F1 + f, 59 + f)
        put(KeyEvent.KEYCODE_F11, 87)
        put(KeyEvent.KEYCODE_F12, 88)
        put(KeyEvent.KEYCODE_CTRL_RIGHT, 97)
        put(KeyEvent.KEYCODE_ALT_RIGHT, 100)
        put(KeyEvent.KEYCODE_MOVE_HOME, 102)
        put(KeyEvent.KEYCODE_DPAD_UP, 103)
        put(KeyEvent.KEYCODE_PAGE_UP, 104)
        put(KeyEvent.KEYCODE_DPAD_LEFT, 105)
        put(KeyEvent.KEYCODE_DPAD_RIGHT, 106)
        put(KeyEvent.KEYCODE_MOVE_END, 107)
        put(KeyEvent.KEYCODE_DPAD_DOWN, 108)
        put(KeyEvent.KEYCODE_PAGE_DOWN, 109)
        put(KeyEvent.KEYCODE_INSERT, 110)
        put(KeyEvent.KEYCODE_FORWARD_DEL, 111)
        put(KeyEvent.KEYCODE_META_LEFT, 125)
        put(KeyEvent.KEYCODE_META_RIGHT, 126)
        put(KeyEvent.KEYCODE_MENU, 127)
    }

    /** The evdev code for an Android key code, or null for keys windowcast does not map. */
    fun evdev(androidKeyCode: Int): Int? = map[androidKeyCode]
}
