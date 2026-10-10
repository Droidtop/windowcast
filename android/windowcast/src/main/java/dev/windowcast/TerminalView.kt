package dev.windowcast

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import android.text.InputType
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager

/**
 * A terminal drawn from a [TerminalSession]'s snapshots, with the keyboard (a hardware one, a
 * gamepad's D-pad and buttons, or the soft keyboard) and vertical drags for the scrollback. It
 * is the baseline view: no selection, no mouse reporting.
 */
class TerminalView(context: Context) : View(context) {
    private val text = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = Typeface.MONOSPACE
        textSize = 14f * resources.displayMetrics.density
    }
    private val fill = Paint()
    private val line = Paint().apply { strokeWidth = 2f; style = Paint.Style.STROKE }
    private var cellWidth = 1f
    private var cellHeight = 1f
    private var baseline = 0f

    @Volatile private var snapshot: TerminalSnapshot? = null
    private var terminal: TerminalSession? = null
    @Volatile private var running = false
    private var dragged = 0f
    private var scrolled = 0

    /** The shell ended (its exit code, -1 for none), or the connection went. Called on the main thread. */
    var onEnded: ((Int) -> Unit)? = null

    /** A program set the clipboard (OSC 52). Called on the main thread. */
    var onClipboard: ((String) -> Unit)? = null

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        setBackgroundColor(DEFAULT_BG)
        val metrics = text.fontMetrics
        cellWidth = text.measureText("M")
        cellHeight = metrics.descent - metrics.ascent
        baseline = -metrics.ascent
    }

    /** How many cells fit the view now. */
    private fun cols() = (width / cellWidth).toInt().coerceAtLeast(1)
    private fun rows() = (height / cellHeight).toInt().coerceAtLeast(1)

    /** Cells that fit a view of [widthPx] by [heightPx], for opening a terminal at the right size. */
    fun cellsFor(widthPx: Int, heightPx: Int): Pair<Int, Int> =
        Pair((widthPx / cellWidth).toInt().coerceAtLeast(20), (heightPx / cellHeight).toInt().coerceAtLeast(5))

    /** Starts showing [session]; [detach] stops. */
    fun attach(session: TerminalSession) {
        detach()
        terminal = session
        running = true
        if (width > 0) session.resize(cols(), rows())
        Thread({
            var seen = 0L
            var told = false
            while (running) {
                val shown = terminal ?: break
                if (shown.waitChange(seen, 500)) {
                    val s = shown.snapshot() ?: break
                    seen = s.version
                    snapshot = s
                    postInvalidate()
                }
                for (clip in shown.takeClipboard()) post { onClipboard?.invoke(clip) }
                val code = shown.exitCode()
                if (code != null && !told) {
                    told = true
                    post { onEnded?.invoke(code) }
                }
            }
        }, "windowcast-terminal").start()
        requestFocus()
    }

    fun detach() {
        running = false
        terminal = null
        snapshot = null
        invalidate()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        terminal?.resize(cols(), rows())
    }

    override fun onDraw(canvas: Canvas) {
        val s = snapshot ?: return
        for ((row, runs) in s.lines.withIndex()) {
            val top = row * cellHeight
            var x = 0f
            for (run in runs) {
                val width = run.text.length * cellWidth
                var fg = run.fg?.let { it or OPAQUE } ?: DEFAULT_FG
                var bg = run.bg?.let { it or OPAQUE } ?: DEFAULT_BG
                if (run.inverse) fg = bg.also { bg = fg }
                if (bg != DEFAULT_BG) {
                    fill.color = bg
                    canvas.drawRect(x, top, x + width, top + cellHeight, fill)
                }
                if (run.text.isNotBlank()) {
                    text.color = fg
                    text.isFakeBoldText = run.bold
                    text.textSkewX = if (run.italic) -0.2f else 0f
                    canvas.drawText(run.text, x, top + baseline, text)
                    if (run.underline) {
                        line.color = fg
                        canvas.drawLine(x, top + cellHeight - 2f, x + width, top + cellHeight - 2f, line)
                    }
                }
                x += width
            }
        }
        if (s.cursorRow >= 0) {
            line.color = DEFAULT_FG
            val left = s.cursorCol * cellWidth
            val top = s.cursorRow * cellHeight
            canvas.drawRect(left, top, left + cellWidth, top + cellHeight, line)
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                dragged = event.y
                requestFocus()
            }
            MotionEvent.ACTION_MOVE -> {
                // Dragging down shows older lines.
                val lines = ((event.y - dragged) / cellHeight).toInt()
                if (lines != 0) {
                    dragged += lines * cellHeight
                    scrolled = (scrolled + lines).coerceAtLeast(0)
                    terminal?.scrollBack(scrolled)
                }
            }
            MotionEvent.ACTION_UP -> {
                if (scrolled == 0) {
                    val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
                    imm.showSoftInput(this, 0)
                }
                performClick()
            }
        }
        return true
    }

    override fun performClick(): Boolean = super.performClick()

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_ACTION_NONE
        return object : BaseInputConnection(this, false) {
            override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                typed(text.toString())
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                repeat(beforeLength.coerceAtLeast(1)) { terminal?.sendKey("Backspace") }
                return true
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean = handleKey(event)
        }
    }

    private fun typed(string: String) {
        if (scrolled != 0) {
            scrolled = 0
            terminal?.scrollBack(0)
        }
        terminal?.sendText(string.replace("\n", "\r"))
    }

    /** Sends a key event to the shell. Returns whether it was used. */
    fun handleKey(event: KeyEvent): Boolean {
        val shell = terminal ?: return false
        if (event.action != KeyEvent.ACTION_DOWN) return NAMED.containsKey(event.keyCode)
        NAMED[event.keyCode]?.let {
            scrolled = 0
            shell.scrollBack(0)
            shell.sendKey(it)
            return true
        }
        if (event.keyCode >= KeyEvent.KEYCODE_F1 && event.keyCode <= KeyEvent.KEYCODE_F12) {
            shell.sendKey("F" + (event.keyCode - KeyEvent.KEYCODE_F1 + 1))
            return true
        }
        if (event.isCtrlPressed && event.keyCode >= KeyEvent.KEYCODE_A && event.keyCode <= KeyEvent.KEYCODE_Z) {
            shell.sendControl('a' + (event.keyCode - KeyEvent.KEYCODE_A))
            return true
        }
        val c = event.unicodeChar
        if (c != 0 && !event.isCtrlPressed) {
            typed(String(Character.toChars(c)))
            return true
        }
        return false
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        keyCode != KeyEvent.KEYCODE_BACK && handleKey(event) || super.onKeyDown(keyCode, event)

    companion object {
        private const val OPAQUE = 0xFF000000.toInt()
        private val DEFAULT_FG = Color.rgb(0xdd, 0xdd, 0xdd)
        private val DEFAULT_BG = Color.rgb(0x10, 0x10, 0x10)

        /** Keys with a name windowcast_terminal_send_key knows. A gamepad's D-pad and A button work as arrows and Enter. */
        private val NAMED: Map<Int, String> = mapOf(
            KeyEvent.KEYCODE_ENTER to "Enter",
            KeyEvent.KEYCODE_NUMPAD_ENTER to "Enter",
            KeyEvent.KEYCODE_BUTTON_A to "Enter",
            KeyEvent.KEYCODE_DEL to "Backspace",
            KeyEvent.KEYCODE_TAB to "Tab",
            KeyEvent.KEYCODE_ESCAPE to "Escape",
            KeyEvent.KEYCODE_DPAD_UP to "Up",
            KeyEvent.KEYCODE_DPAD_DOWN to "Down",
            KeyEvent.KEYCODE_DPAD_LEFT to "Left",
            KeyEvent.KEYCODE_DPAD_RIGHT to "Right",
            KeyEvent.KEYCODE_MOVE_HOME to "Home",
            KeyEvent.KEYCODE_MOVE_END to "End",
            KeyEvent.KEYCODE_PAGE_UP to "PageUp",
            KeyEvent.KEYCODE_PAGE_DOWN to "PageDown",
            KeyEvent.KEYCODE_INSERT to "Insert",
            KeyEvent.KEYCODE_FORWARD_DEL to "Delete",
        )
    }
}
