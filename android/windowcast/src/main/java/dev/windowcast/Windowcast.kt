package dev.windowcast

import java.io.Closeable
import java.io.File
import java.io.IOException
import org.json.JSONObject

/** Video codecs, numbered as in windowcast.h. */
enum class Codec(val id: Int, val mime: String) {
    H264(0, "video/avc"),
    H265(1, "video/hevc"),
    AV1(2, "video/av01");

    companion object {
        fun fromId(id: Int): Codec? = entries.firstOrNull { it.id == id }

        fun fromName(name: String?): Codec? = when (name) {
            "H264" -> H264
            "H265" -> H265
            "Av1" -> AV1
            else -> null
        }
    }
}

data class WindowInfo(
    val id: Long,
    val title: String,
    val appId: String,
    val width: Int,
    val height: Int,
    val focused: Boolean,
    val content: String,
)

/** Session events, as client-core reports them. */
sealed interface Event {
    data class Windows(val windows: List<WindowInfo>) : Event
    data class StreamStarted(val window: Long, val backend: String, val codec: Codec?) : Event
    data class StreamRefused(val window: Long, val reason: String) : Event
    data class StreamStopped(val window: Long) : Event
    data class WindowResized(val window: Long, val width: Int, val height: Int) : Event
    data class WindowFocused(val window: Long) : Event
    data class Clipboard(val text: String) : Event
    data object Closed : Event

    companion object {
        fun parse(json: String): Event? {
            val o = JSONObject(json)
            return when (o.getString("type")) {
                "windows" -> {
                    val list = o.getJSONArray("windows")
                    Windows((0 until list.length()).map { i ->
                        val w = list.getJSONObject(i)
                        WindowInfo(
                            id = w.getLong("id"),
                            title = w.getString("title"),
                            appId = w.getString("app_id"),
                            width = w.getInt("width"),
                            height = w.getInt("height"),
                            focused = w.getBoolean("focused"),
                            content = w.getString("content"),
                        )
                    })
                }
                "stream_started" -> StreamStarted(
                    o.getLong("window"),
                    o.getString("backend"),
                    Codec.fromName(o.optString("codec").takeIf { o.has("codec") && !o.isNull("codec") }),
                )
                "stream_refused" -> StreamRefused(o.getLong("window"), o.getString("reason"))
                "stream_stopped" -> StreamStopped(o.getLong("window"))
                "window_resized" -> WindowResized(o.getLong("window"), o.getInt("width"), o.getInt("height"))
                "window_focused" -> WindowFocused(o.getLong("window"))
                "clipboard" -> Clipboard(o.getString("text"))
                "closed" -> Closed
                else -> null
            }
        }
    }
}

/** One client identity, kept in [dataDir] (app-private storage). */
class WindowcastClient(dataDir: File) : Closeable {
    private var handle: Long = Native.clientNew(dataDir.absolutePath)

    init {
        if (handle == 0L) throw IOException("cannot open the windowcast identity in $dataDir")
    }

    val peerId: String get() = Native.clientPeerId(handle)

    /**
     * Connects to a host agent at [address] ("HOST:PORT"), pairing with [pin]
     * the first time or resuming with the pinned identity when it is null.
     * Blocks: call it off the main thread.
     */
    fun connect(address: String, pin: String?): WindowcastSession {
        val session = Native.connect(handle, address, pin)
        if (session == 0L) throw IOException(Native.lastError())
        return WindowcastSession(session)
    }

    override fun close() {
        if (handle != 0L) Native.clientFree(handle)
        handle = 0L
    }
}

/** Shows one streamed window: [WindowDecoder] for video, [WindowPictures] for RDP. */
interface WindowRenderer {
    fun start()
    fun stop()
}

/** A connected session. Every call blocks; none may run on the main thread. */
class WindowcastSession internal constructor(internal val handle: Long) : Closeable {
    @Volatile private var closed = false

    val hostId: String = Native.sessionHost(handle)
    val paired: Boolean = Native.sessionPaired(handle)

    fun requestWindows() {
        Native.requestWindows(handle)
    }

    /**
     * Says this client shows windows as pictures ([WindowPictures]), so
     * windows the rules send to RDP come over RDP (a stream_started with
     * backend "Rdp").
     */
    fun acceptPictures(on: Boolean) {
        Native.acceptPictures(handle, on)
    }

    /** Asks to stream [window], decodable in [codecs], most preferred first. */
    fun startWindow(window: Long, codecs: List<Codec>) {
        Native.startWindow(handle, window, codecs.map { it.id }.toIntArray())
    }

    fun stopWindow(window: Long) {
        Native.stopWindow(handle, window)
    }

    /** Sends one input event (see [Input]). */
    fun send(input: Input) {
        Native.sendInput(handle, input.json)
    }

    /** Gives the host this device's clipboard text. */
    fun setClipboard(text: String) {
        Native.setClipboard(handle, text)
    }

    /** The next event, or null after [timeoutMs] without one. */
    fun nextEvent(timeoutMs: Int): Event? =
        Native.nextEvent(handle, timeoutMs)?.let { Event.parse(it) }

    override fun close() {
        if (!closed) {
            closed = true
            Native.sessionFree(handle)
        }
    }
}

/**
 * Input for the host, in the form client-core's C interface takes
 * (windowcast.h, `windowcast_session_send_input`). Pointer and touch
 * positions are fractions of the streamed picture, 0 to 1.
 */
class Input private constructor(internal val json: String) {
    enum class Touch { Start, Move, End, Cancel }
    enum class Button { Left, Right, Middle, Back, Forward }

    companion object {
        private fun wrap(kind: String, body: JSONObject) = Input(JSONObject().put(kind, body).toString())

        fun pointerMove(window: Long, x: Float, y: Float) =
            wrap("PointerMove", JSONObject().put("window", window).put("x", x.toDouble()).put("y", y.toDouble()))

        fun pointerButton(window: Long, button: Button, pressed: Boolean) =
            wrap("PointerButton", JSONObject().put("window", window).put("button", button.name).put("pressed", pressed))

        fun scroll(window: Long, dx: Float, dy: Float) =
            wrap("PointerScroll", JSONObject().put("window", window).put("dx", dx.toDouble()).put("dy", dy.toDouble()))

        /** [evdevKeycode]: see [Keys.evdev]. */
        fun key(evdevKeycode: Int, pressed: Boolean) =
            wrap("Key", JSONObject().put("keycode", evdevKeycode).put("pressed", pressed))

        fun text(text: String) = wrap("Text", JSONObject().put("text", text))

        fun touch(window: Long, id: Int, x: Float, y: Float, phase: Touch) =
            wrap(
                "Touch",
                JSONObject().put("window", window).put("id", id).put("x", x.toDouble()).put("y", y.toDouble())
                    .put("phase", phase.name),
            )

        fun gamepad(pad: Int, state: GamepadState) = wrap(
            "Gamepad",
            JSONObject().put("pad", pad).put(
                "state",
                JSONObject().put("buttons", state.buttons).put("left_x", state.leftX).put("left_y", state.leftY)
                    .put("right_x", state.rightX).put("right_y", state.rightY)
                    .put("left_trigger", state.leftTrigger).put("right_trigger", state.rightTrigger),
            ),
        )

        fun gamepadGone(pad: Int) = wrap("GamepadGone", JSONObject().put("pad", pad))
    }
}

/** An Xbox-layout gamepad; button bits as XInput's (windowcast_protocol::GamepadButtons). */
data class GamepadState(
    val buttons: Int = 0,
    val leftX: Int = 0,
    val leftY: Int = 0,
    val rightX: Int = 0,
    val rightY: Int = 0,
    val leftTrigger: Int = 0,
    val rightTrigger: Int = 0,
) {
    companion object {
        const val DPAD_UP = 0x0001
        const val DPAD_DOWN = 0x0002
        const val DPAD_LEFT = 0x0004
        const val DPAD_RIGHT = 0x0008
        const val START = 0x0010
        const val BACK = 0x0020
        const val LEFT_THUMB = 0x0040
        const val RIGHT_THUMB = 0x0080
        const val LEFT_SHOULDER = 0x0100
        const val RIGHT_SHOULDER = 0x0200
        const val GUIDE = 0x0400
        const val A = 0x1000
        const val B = 0x2000
        const val X = 0x4000
        const val Y = 0x8000
    }
}
