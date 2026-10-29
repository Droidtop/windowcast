package dev.windowcast

import java.io.Closeable
import java.io.File
import java.io.IOException
import java.nio.ByteBuffer
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
    /** The window that owns this one (a dialog's, popup's or menu's program window), or null. */
    val owner: Long? = null,
    /** "normal", "dialog", "popup" or "menu". */
    val kind: String = "normal",
    /** Its top-left on the host's desktop, when the host knows it. */
    val position: Pair<Int, Int>? = null,
)

/** What a stream is being sent at and the network, as the host sees it (client-core's `StreamQuality`). */
data class Quality(
    /** The rate the host holds the encoder to, kilobits a second; null while only the settings hold it. */
    val targetKbps: Int?,
    /** What actually went out over the last second or so. */
    val sentKbps: Int,
    val fps: Int,
    val width: Int,
    val height: Int,
    /** Packets the client lost, percent. */
    val lossPercent: Float,
    /** Round trip over the session, milliseconds. */
    val rttMs: Int?,
) {
    /**
     * In a few words, as the reference app shows it: what the stream is held to, its size and rate,
     * what is sent, and the network. For a person to read, e.g.
     * "held to 4.0 Mbit/s, 1280x720 at 60 fps, sending 3.2 Mbit/s; loss 0.4%, round trip 12 ms".
     */
    fun describe(): String {
        val held = targetKbps?.let { "held to ${mbit(it)} Mbit/s" } ?: "as set"
        val rtt = rttMs?.let { ", round trip $it ms" } ?: ""
        return "$held, ${width}x$height at $fps fps, sending ${mbit(sentKbps)} Mbit/s; loss ${"%.1f".format(java.util.Locale.US, lossPercent)}%$rtt"
    }

    private fun mbit(kbps: Int): String = "%.1f".format(java.util.Locale.US, kbps / 1000.0)
}

/** Session events, as client-core reports them. */
sealed interface Event {
    data class Windows(val windows: List<WindowInfo>) : Event
    data class StreamStarted(val window: Long, val backend: String, val codec: Codec?) : Event
    data class StreamRefused(val window: Long, val reason: String) : Event
    data class StreamStopped(val window: Long) : Event
    data class CarrierStarted(val window: Long, val generation: Int, val backend: String, val codec: Codec?) : Event
    data class CarrierRefused(val window: Long, val generation: Int, val reason: String) : Event
    data class WindowResized(val window: Long, val width: Int, val height: Int) : Event
    data class WindowFocused(val window: Long) : Event
    /**
     * The host computer was locked or unlocked (its windows keep streaming while it is locked), or
     * its private session changed: [privateSession] is "off" (not offered), "unavailable" (why in
     * [privateSessionReason]), "available" ([WindowcastSession.privateSession] starts one) or
     * "active".
     */
    data class HostState(val locked: Boolean, val privateSession: String, val privateSessionReason: String?) : Event
    /** A private session did not start: why, and whether asking with the Windows password may work. */
    data class PrivateSessionRefused(val reason: String, val passwordNeeded: Boolean) : Event
    /**
     * A window's compatibility mode now: one line per way a third-party client (RDP, Moonlight,
     * VNC) reaches it, for a person to read (none: off), or why it could not be turned on.
     */
    data class Compatibility(val window: Long, val endpoints: List<String>, val refused: String?) : Event
    /**
     * The host's pointer over a streamed window: [x] and [y] are fractions of its picture, and
     * [shape] names an image [WindowcastSession.cursorImage] gives. The host leaves the pointer out
     * of its pictures, so the client draws it.
     */
    data class Cursor(val window: Long, val x: Float, val y: Float, val visible: Boolean, val shape: Long) : Event
    data class Clipboard(val text: String) : Event

    /**
     * What a window's stream is sent at now and the network the host sees (adaptive quality,
     * Droidtop/tracker#114): on every change and every few seconds.
     */
    data class StreamQuality(val window: Long, val quality: Quality) : Event

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
                            owner = if (w.isNull("owner")) null else w.getLong("owner"),
                            kind = w.optString("kind", "normal"),
                            position = w.optJSONArray("position")?.let { p -> p.getInt(0) to p.getInt(1) },
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
                "carrier_started" -> CarrierStarted(
                    o.getLong("window"),
                    o.getInt("generation"),
                    o.getString("backend"),
                    Codec.fromName(o.optString("codec").takeIf { o.has("codec") && !o.isNull("codec") }),
                )
                "carrier_refused" -> CarrierRefused(o.getLong("window"), o.getInt("generation"), o.getString("reason"))
                "window_resized" -> WindowResized(o.getLong("window"), o.getInt("width"), o.getInt("height"))
                "window_focused" -> WindowFocused(o.getLong("window"))
                "host_state" -> HostState(
                    o.getBoolean("locked"),
                    o.optString("private_session", "off"),
                    if (o.isNull("private_session_reason")) null else o.optString("private_session_reason"),
                )
                "private_session_refused" -> PrivateSessionRefused(o.getString("reason"), o.getBoolean("password_needed"))
                "compatibility" -> {
                    val lines = o.getJSONArray("described")
                    Compatibility(
                        o.getLong("window"),
                        (0 until lines.length()).map { lines.getString(it) },
                        if (o.isNull("refused")) null else o.optString("refused"),
                    )
                }
                "cursor" -> Cursor(
                    o.getLong("window"),
                    o.getDouble("x").toFloat(),
                    o.getDouble("y").toFloat(),
                    o.getBoolean("visible"),
                    o.getLong("shape"),
                )
                "clipboard" -> Clipboard(o.getString("text"))
                "stream_quality" -> o.getJSONObject("quality").let { q ->
                    StreamQuality(
                        o.getLong("window"),
                        Quality(
                            targetKbps = if (q.isNull("target_kbps")) null else q.getInt("target_kbps"),
                            sentKbps = q.getInt("sent_kbps"),
                            fps = q.getInt("fps"),
                            width = q.getInt("width"),
                            height = q.getInt("height"),
                            lossPercent = q.getDouble("loss_percent").toFloat(),
                            rttMs = if (q.isNull("rtt_ms")) null else q.getInt("rtt_ms"),
                        ),
                    )
                }
                "closed" -> Closed
                else -> null
            }
        }
    }
}

/** One client identity, kept in [dataDir] (app-private storage). */
/** A RemoteApp launch needs the Windows password of [user]: ask, and launch again with it. */
class PasswordNeededException(val user: String) :
    IOException("Remote Desktop needs the Windows password of $user")

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

    /**
     * Logs in to an SSH server and opens a shell. [key] is a private key in PEM form (then
     * [secret] is unused); otherwise [secret] is the password. A server not pinned yet throws
     * [UntrustedHostKey] under [HostKeyPolicy.PINNED], carrying the key it presented.
     * Blocks: call it off the main thread.
     */
    fun sshTerminal(
        host: String,
        port: Int,
        user: String,
        secret: String,
        key: String? = null,
        passphrase: String? = null,
        policy: HostKeyPolicy = HostKeyPolicy.PINNED,
        fingerprint: String? = null,
        cols: Int,
        rows: Int,
    ): TerminalSession {
        val terminal = Native.sshTerminal(
            handle, host, port, user, if (key != null) 1 else 0, key ?: secret, passphrase,
            policy.id, fingerprint, cols, rows,
        )
        if (terminal == 0L) {
            val seen = Native.lastFingerprint()
            if (seen.isNotEmpty()) throw UntrustedHostKey(seen, Native.lastError())
            throw IOException(Native.lastError())
        }
        return TerminalSession(terminal)
    }

    /** Asks the host at [address] what account sign-ins it takes, without signing in. Blocks. */
    fun signInOptions(address: String): SignInOptions =
        SignInOptions.parse(Native.signInOptions(handle, address) ?: throw IOException(Native.lastError()))

    /**
     * Signs in to the host at [address] and connects. The credential goes only to a host this
     * client trusts, or to the one whose identity the user confirmed by its fingerprint and that
     * is passed as [acceptHost] ([SignInOptions.hostId]). Later connections resume with
     * [connect] and no PIN while the host keeps the registration. Blocks.
     */
    fun connectAccount(address: String, signIn: SignIn, acceptHost: String?): WindowcastSession {
        val session = Native.connectAccount(handle, address, signIn.json, acceptHost)
        if (session == 0L) throw IOException(Native.lastError())
        return WindowcastSession(session)
    }

    /**
     * Starts a sign-in with [provider] in the browser. Only for a host the user trusts: the
     * provider is the host's to name. Blocks while it reads the provider's metadata.
     */
    fun oidcBrowser(provider: OidcProvider): OidcBrowserSignIn {
        val url = arrayOfNulls<String>(1)
        val signIn = Native.oidcBrowserStart(handle, provider.json, url)
        if (signIn == 0L) throw IOException(Native.lastError())
        return OidcBrowserSignIn(signIn, url[0]!!)
    }

    /** Starts a sign-in with [provider] finished on another device. As [oidcBrowser] otherwise. */
    fun oidcDevice(provider: OidcProvider): OidcDeviceSignIn {
        val shown = arrayOfNulls<String>(1)
        val signIn = Native.oidcDeviceStart(handle, provider.json, shown)
        if (signIn == 0L) throw IOException(Native.lastError())
        return OidcDeviceSignIn.parse(signIn, shown[0]!!)
    }

    override fun close() {
        if (handle != 0L) Native.clientFree(handle)
        handle = 0L
    }
}

/** A pointer shape: [argb] pixels, rows from the top, for a Bitmap, and the point that is its position. */
class CursorImage(val width: Int, val height: Int, val hotspotX: Int, val hotspotY: Int, val argb: IntArray)

/** Shows one streamed window: [WindowDecoder] for video, [WindowPictures] for RDP. */
interface WindowRenderer {
    fun start()
    fun stop()
}

/** A connected session. Every call blocks; none may run on the main thread. */
class WindowcastSession internal constructor(handle: Long) : Closeable, EventSource {
    // Calls run outside any lock and [close] frees the session when the last one has left, so a
    // reader inside [nextEvent] never meets a freed session (it returns null from then on).
    private val native = NativeHandle(handle, Native::sessionFree)

    val hostId: String = Native.sessionHost(handle)
    val paired: Boolean = Native.sessionPaired(handle)

    /** True until [close]. */
    override val isOpen: Boolean get() = native.isOpen

    /** Runs [block] with the session's native handle, or returns [default] once closed. */
    internal fun <T> withHandle(default: T, block: (Long) -> T): T = native.use(default, block)

    fun requestWindows() {
        withHandle(Unit) { Native.requestWindows(it) }
    }

    /**
     * Says this client shows windows as pictures ([WindowPictures]), so
     * windows the rules send to RDP come over RDP (a stream_started with
     * backend "Rdp").
     */
    fun acceptPictures(on: Boolean) {
        withHandle(Unit) { Native.acceptPictures(it, on) }
    }

    /**
     * Whether the dialogs, popups and menus a shown window owns are shown too, each as its own
     * window, as they open (on by default): each arrives as a [Event.StreamStarted] for its own
     * id, and its [WindowInfo.owner] names the window it belongs to.
     */
    fun followPopups(on: Boolean) {
        withHandle(Unit) { Native.followPopups(it, on) }
    }

    /** Asks to stream [window], decodable in [codecs], most preferred first. */
    fun startWindow(window: Long, codecs: List<Codec>) {
        withHandle(Unit) { Native.startWindow(it, window, codecs.map { c -> c.id }.toIntArray()) }
    }

    /**
     * Switches a streamed window to [backend] ("Native", "Rdp", ...) without a break: the new
     * carrier arrives as [Event.CarrierStarted]; show it beside the old one, swap on its first
     * picture and call [carrierShown]. [Event.CarrierRefused] says it did not happen. Returns the
     * new generation.
     */
    fun switchWindow(window: Long, backend: String, codecs: List<Codec>): Int {
        val generation = withHandle(Native.ERROR) {
            Native.switchWindow(it, window, backend, codecs.map { c -> c.id }.toIntArray())
        }
        if (generation < 0) throw IOException(if (isOpen) Native.lastError() else "the session is closed")
        return generation.toInt()
    }

    /**
     * Whether a window's carrier switches by itself as what it shows and the connection change
     * (on by default); its switches arrive as [Event.CarrierStarted], as for [switchWindow].
     */
    fun autoSwitch(on: Boolean) {
        withHandle(Unit) { Native.autoSwitch(it, on) }
    }

    /** The first picture of carrier [generation] was shown: it replaces the old one. */
    fun carrierShown(window: Long, generation: Int) {
        withHandle(Unit) { Native.carrierShown(it, window, generation) }
    }

    fun stopWindow(window: Long) {
        withHandle(Unit) { Native.stopWindow(it, window) }
    }

    /** Sends one input event (see [Input]). */
    fun send(input: Input) {
        withHandle(Unit) { Native.sendInput(it, input.json) }
    }

    /** Gives the host this device's clipboard text. */
    fun setClipboard(text: String) {
        withHandle(Unit) { Native.setClipboard(it, text) }
    }

    /** A shell on the host, drawn on a screen of [cols] by [rows] cells. The host may refuse (IOException). */
    fun openTerminal(cols: Int, rows: Int): TerminalSession {
        val terminal = withHandle(0L) { Native.openTerminal(it, cols, rows) }
        if (terminal == 0L) throw IOException(if (isOpen) Native.lastError() else "the session is closed")
        return TerminalSession(terminal)
    }

    /**
     * Starts an application on the host: [argv] is the program and its arguments. Its windows
     * arrive in the window list. Returns the process id (0 if the host does not know it, and for
     * a program the host runs as a RemoteApp, whose windows join the list as it opens them).
     * A RemoteApp may need the user's Windows password: [PasswordNeededException] names the
     * user, and launching again with [password] uses it.
     */
    fun launch(argv: List<String>, password: String? = null): Long {
        val pid = withHandle(Native.ERROR) {
            Native.launch(it, org.json.JSONArray(argv).toString(), password)
        }
        if (pid == Native.PASSWORD_NEEDED) throw PasswordNeededException(Native.lastError())
        if (pid < 0) throw IOException(if (isOpen) Native.lastError() else "the session is closed")
        return pid
    }

    /**
     * Puts a window this session streams in compatibility mode ([on]) or takes it out: third-party
     * clients (RDP, Moonlight, VNC) can then reach it too. Answered by [Event.Compatibility].
     */
    fun setCompatibility(window: Long, on: Boolean) {
        withHandle(Unit) { Native.setCompatibility(it, window, on) }
    }

    /**
     * Starts or ends a private session on the host: its programs run where nobody at the computer
     * sees them. [password] is the Windows password of the user the host runs as, for a host that
     * asks for it ([Event.PrivateSessionRefused.passwordNeeded]).
     */
    fun privateSession(start: Boolean, password: String? = null) {
        withHandle(Unit) { Native.privateSession(it, start, password) }
    }

    /** The pointer shape [shape] names ([Event.Cursor]), or null for one the host has not sent. */
    fun cursorImage(shape: Long): CursorImage? {
        var buffer = ByteBuffer.allocateDirect(64 * 64 * 4)
        val info = IntArray(4)
        var len = withHandle(Native.ERROR) { Native.cursorImage(it, shape, buffer, info) }
        if (len == Native.BUFFER_TOO_SMALL) {
            buffer = ByteBuffer.allocateDirect(info[0] * info[1] * 4)
            len = withHandle(Native.ERROR) { Native.cursorImage(it, shape, buffer, info) }
        }
        if (len < 0) return null
        val argb = IntArray(info[0] * info[1]) { i ->
            val r = buffer.get(i * 4).toInt() and 0xff
            val g = buffer.get(i * 4 + 1).toInt() and 0xff
            val b = buffer.get(i * 4 + 2).toInt() and 0xff
            val a = buffer.get(i * 4 + 3).toInt() and 0xff
            (a shl 24) or (r shl 16) or (g shl 8) or b
        }
        return CursorImage(info[0], info[1], info[2], info[3], argb)
    }

    /** The next event, or null after [timeoutMs] without one, and for good once the session is closed. */
    override fun nextEvent(timeoutMs: Int): Event? =
        withHandle<String?>(null) { Native.nextEvent(it, timeoutMs) }?.let { Event.parse(it) }

    /** Never waits: the session is freed by the last call still running on it. */
    override fun close() {
        native.close()
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
