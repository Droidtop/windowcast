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

/** A connected session. Every call blocks; none may run on the main thread. */
class WindowcastSession internal constructor(internal val handle: Long) : Closeable {
    @Volatile private var closed = false

    val hostId: String = Native.sessionHost(handle)
    val paired: Boolean = Native.sessionPaired(handle)

    fun requestWindows() {
        Native.requestWindows(handle)
    }

    /** Asks to stream [window], decodable in [codecs], most preferred first. */
    fun startWindow(window: Long, codecs: List<Codec>) {
        Native.startWindow(handle, window, codecs.map { it.id }.toIntArray())
    }

    fun stopWindow(window: Long) {
        Native.stopWindow(handle, window)
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
