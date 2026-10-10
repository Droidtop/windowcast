package dev.windowcast

import java.io.Closeable
import java.io.IOException
import org.json.JSONArray
import org.json.JSONObject

/** A run of cells with one look. Colours are 0xRRGGBB, or null for the view's default. */
data class TerminalRun(
    val text: String,
    val fg: Int?,
    val bg: Int?,
    val bold: Boolean,
    val italic: Boolean,
    val underline: Boolean,
    /** Foreground and background swap. */
    val inverse: Boolean,
)

/** What a view draws: the rows as runs, the cursor (-1 when hidden) and a version that changes with the screen. */
class TerminalSnapshot(
    val cols: Int,
    val rows: Int,
    val lines: List<List<TerminalRun>>,
    val cursorRow: Int,
    val cursorCol: Int,
    val version: Long,
) {
    companion object {
        fun parse(json: String): TerminalSnapshot {
            val o = JSONObject(json)
            val lines = o.getJSONArray("lines")
            val cursor = o.optJSONArray("cursor")
            return TerminalSnapshot(
                cols = o.getInt("cols"),
                rows = o.getInt("rows"),
                lines = (0 until lines.length()).map { row ->
                    val runs = lines.getJSONArray(row)
                    (0 until runs.length()).map { i ->
                        val r = runs.getJSONObject(i)
                        TerminalRun(
                            text = r.getString("text"),
                            fg = if (r.isNull("fg")) null else r.getInt("fg"),
                            bg = if (r.isNull("bg")) null else r.getInt("bg"),
                            bold = r.getBoolean("bold"),
                            italic = r.getBoolean("italic"),
                            underline = r.getBoolean("underline"),
                            inverse = r.getBoolean("inverse"),
                        )
                    }
                },
                cursorRow = cursor?.getInt(0) ?: -1,
                cursorCol = cursor?.getInt(1) ?: -1,
                version = o.getLong("version"),
            )
        }
    }
}

/** An SSH server that is not trusted yet: the key it presented, for the user to confirm. */
class UntrustedHostKey(val fingerprint: String, message: String) : IOException(message)

/** How an SSH server not pinned yet is dealt with. */
enum class HostKeyPolicy(internal val id: Int) {
    /** Pin whatever it presents. */
    FIRST_USE(0),

    /** Refuse it ([UntrustedHostKey] carries its key). */
    PINNED(1),

    /** Accept only the fingerprint given. */
    FINGERPRINT(2),
}

/**
 * A shell, on the host of a windowcast session or on an SSH server, drawn by the
 * library's screen model: send input, read [snapshot]s. Every call blocks briefly;
 * none may run on the main thread except the cheap input calls.
 */
class TerminalSession internal constructor(handle: Long) : Closeable {
    private var handle: Long = handle
    private val lock = Any()

    private inline fun <T> live(default: T, block: (Long) -> T): T =
        synchronized(lock) { if (handle == 0L) default else block(handle) }

    fun sendText(text: String) {
        live(0L) { Native.terminalSendText(it, text) }
    }

    /** Enter, Backspace, Tab, Escape, Up, Down, Left, Right, Home, End, PageUp, PageDown, Insert, Delete, F1..F12. */
    fun sendKey(name: String) {
        live(0L) { Native.terminalSendKey(it, name) }
    }

    /** Ctrl and a letter (or one of @[\]^_). */
    fun sendControl(c: Char) {
        live(0L) { Native.terminalSendControl(it, c.code) }
    }

    /** Pastes text, bracketed when the program asked for that. */
    fun paste(text: String) {
        live(0L) { Native.terminalPaste(it, text) }
    }

    fun resize(cols: Int, rows: Int) {
        live(0L) { Native.terminalResize(it, cols, rows) }
    }

    fun scrollBack(lines: Int) {
        live(0L) { Native.terminalScroll(it, lines) }
    }

    /** Waits up to [timeoutMs] for the screen to change since [seenVersion]. */
    fun waitChange(seenVersion: Long, timeoutMs: Int): Boolean =
        live(false) { Native.terminalWait(it, seenVersion, timeoutMs) > 0 }

    fun snapshot(): TerminalSnapshot? = live(null) { h ->
        Native.terminalSnapshot(h)?.let { TerminalSnapshot.parse(it) }
    }

    /** Texts programs put on the clipboard (OSC 52) since the last call. */
    fun takeClipboard(): List<String> = live(emptyList()) { h ->
        val json = Native.terminalTakeClipboard(h) ?: return@live emptyList()
        val array = JSONArray(json)
        (0 until array.length()).map { array.getString(it) }
    }

    /** Null while the shell runs; its exit code (-1 if it had none) once it ended. */
    fun exitCode(): Int? = live(null) { h ->
        val code = IntArray(1)
        if (Native.terminalEnded(h, code) == Native.ENDED) code[0] else null
    }

    override fun close() {
        synchronized(lock) {
            if (handle != 0L) Native.terminalFree(handle)
            handle = 0L
        }
    }
}
