package dev.windowcast.viewer

import dev.windowcast.Letterbox
import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.content.Context
import android.content.Intent
import android.os.Bundle
import dev.windowcast.SignIn
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Menu
import android.view.MenuItem
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.ImageView
import android.graphics.Bitmap
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import dev.windowcast.AudioPlayer
import dev.windowcast.CarrierSwitcher
import dev.windowcast.Codec
import dev.windowcast.ConnectRequest
import dev.windowcast.Event
import dev.windowcast.EventPump
import dev.windowcast.SessionHolder
import dev.windowcast.SessionListener
import dev.windowcast.SignInPlan
import dev.windowcast.WantedWindow
import dev.windowcast.WatchSet
import dev.windowcast.WindowDecoder
import dev.windowcast.WindowRenderer
import dev.windowcast.WindowInfo
import dev.windowcast.WindowLayout
import dev.windowcast.WindowRenderers
import dev.windowcast.InputAdapter
import dev.windowcast.WindowcastClient
import dev.windowcast.WindowcastSession
import dev.windowcast.Gamepads
import dev.windowcast.Input
import dev.windowcast.HostKeyPolicy
import dev.windowcast.Microphone
import dev.windowcast.TerminalSession
import dev.windowcast.TerminalView
import dev.windowcast.UntrustedHostKey
import android.Manifest
import android.content.pm.PackageManager
import android.content.ClipData
import android.content.ClipboardManager
import android.view.KeyEvent
import android.view.MotionEvent
import java.io.File
import java.io.IOException
import java.util.concurrent.Executors

/**
 * A minimal windowcast client for trying the library against a host:
 * connect (PIN the first time), pick a window, watch it. Everything it
 * does goes through the windowcast Android library.
 */
class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    /** Sign-ins wait for a person for minutes: they must not hold up [worker]. */
    private val signInWorker = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())
    /** Input goes out in order, off the main thread. */
    private val inputWorker = Executors.newSingleThreadExecutor()
    private val gamepads = Gamepads { send(it) }
    private lateinit var clipboard: ClipboardManager
    /** The last text the host put on the clipboard, so it is not sent back. */
    private var fromHost: String? = null

    private lateinit var address: EditText
    private lateinit var pin: EditText
    private lateinit var status: TextView
    /** One row per window the host listed. */
    private lateinit var list: LinearLayout
    private lateinit var surface: SurfaceView

    /** The frame the form, the stream surfaces and the terminal sit in. */
    private lateinit var stage: FrameLayout

    /**
     * A second surface stacked with [surface]: a carrier switch shows the new carrier here and
     * swaps the two on its first picture (docs/BACKENDS.md, "One window, any carrier").
     */
    private lateinit var spare: SurfaceView

    /** The state of a carrier switch and the switches this host refused (Droidtop/tracker#471), kept by the library. */
    private val switcher = CarrierSwitcher<WindowRenderer> { session?.hostId }

    private val switching get() = switcher.pending

    /**
     * The host's pointer, drawn over the watched window where the host says it is (the host
     * leaves it out of its pictures, so it looks the same on every carrier), and its shapes.
     */
    private lateinit var pointer: ImageView
    private val pointerShapes = mutableMapOf<Long, Pair<Bitmap, Pair<Int, Int>>>()
    private var lastCursor: Event.Cursor? = null

    /** The host's private session as it last said: "off", "unavailable", "available" or "active". */
    private var hostPrivate = "off"
    private var hostLocked = false

    /** The watched window is in compatibility mode. */
    private var compatibility = false

    /**
     * The renderer drawing on each surface: stopped when that surface is destroyed, before the
     * callback returns, so no decoder writes to a surface that is gone (#463).
     */
    private val drawing = mutableMapOf<SurfaceView, WindowRenderer>()

    private fun watchSurface(view: SurfaceView) {
        view.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {}
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                drawing.remove(view)?.stop()
            }
        })
    }

    /** The carrier the watched window is on ("Native", "Rdp"), for the switch menu. */
    private var carrier: String? = null
    private lateinit var terminalView: TerminalView
    private lateinit var launchLine: EditText
    private lateinit var sshUser: EditText
    private lateinit var sshHost: EditText
    private lateinit var sshPort: EditText
    private lateinit var sshPassword: EditText
    private lateinit var sshButton: Button
    private var terminalSession: TerminalSession? = null
    /** The fingerprint of an SSH server the user was shown and has not trusted yet. */
    private var pendingFingerprint: String? = null
    private lateinit var form: ScrollView
    /** Shown while a browser sign-in waits; gives it up. */
    private lateinit var cancelBrowserSignIn: Button
    private lateinit var signIns: SignInFlow
    private var clipboardListener: ClipboardManager.OnPrimaryClipChangedListener? = null
    /** Counts browser sign-ins; a finished one acts only if it is still the latest, so Cancel can drop one. */
    /** Set once the system has asked for the state to keep across a recreation (onDestroy keeps the session then). */
    private var keepSession = false
    private lateinit var sendMicrophone: CheckBox
    @Volatile private var microphone: Microphone? = null

    private var client: WindowcastClient? = null
    /** The one current session and the reader following it (the library's, shared with droidtop). */
    private val sessions = SessionHolder<WindowcastSession>()
    private val session: WindowcastSession? get() = sessions.session
    private var decoder: WindowRenderer? = null

    /** A shown window's dialog, popup or menu, in a floating window of its own. */
    private class PopupView(val dialog: Dialog, var renderer: WindowRenderer?)

    /** The open popups, by window id. */
    private val popups = mutableMapOf<Long, PopupView>()

    /** Which windows are showing, and what asking to watch one does (docs/ANDROID-VIEWER.md). */
    private val watchSet = WatchSet()

    /**
     * A window watched beside the one that fills the screen: a floating window with its own
     * surface, decoder and sound, kept until it is closed, the host closes the window, or the
     * session ends.
     */
    private class StreamWindow(val dialog: Dialog, var renderer: WindowRenderer?, var audio: AudioPlayer?)

    private val extras = mutableMapOf<Long, StreamWindow>()
    private var audio: AudioPlayer? = null
    @Volatile private var soundPackets = 0L
    private var windows: List<WindowInfo> = emptyList()

    /** What launching a program said, kept in front of the next window-count line so the list does not wipe it. */
    private val launchNote = LaunchNote()
    private var watching: WindowInfo? = null

    /** The decoder's line (frames or pictures) and the host's quality line, which together are the action-bar title while watching. */
    private var decoderLine = ""
    private var qualityLine: String? = null

    private fun showStreamTitle(window: WindowInfo) {
        title = "${window.title}: $decoderLine" + (qualityLine?.let { " | $it" } ?: "")
    }

    /**
     * The window another app asked for ([ConnectRequest.window], part of its title), watched as
     * soon as the host lists it (see [WantedWindow]).
     */
    private var wanted: WantedWindow? = null
    /** Reads the current session's events; replacing or clearing it stops the old one. */

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("viewer", Context.MODE_PRIVATE)

        address = EditText(this).apply {
            hint = "Host address (HOST:PORT)"
            setText(prefs.getString("address", ""))
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
        }
        pin = EditText(this).apply {
            hint = "PIN shown by the host (first time only)"
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        val connect = Button(this).apply {
            text = "Connect"
            setOnClickListener {
                prefs.edit().putString("address", address.text.toString()).apply()
                connect(address.text.toString().trim(), pin.text.toString().trim().ifEmpty { null })
            }
        }
        val signInButton = Button(this).apply {
            text = "Sign in with an account"
            setOnClickListener {
                prefs.edit().putString("address", address.text.toString()).apply()
                signIns.signIn(address.text.toString().trim())
            }
        }
        cancelBrowserSignIn = Button(this).apply {
            text = "Cancel sign-in"
            visibility = View.GONE
            setOnClickListener { signIns.giveUpBrowserSignIn(SignInPlan.CANCELLED) }
        }
        status = TextView(this).apply { text = "Not connected" }
        signIns = SignInFlow(
            this, main, status, cancelBrowserSignIn, { client }, worker, signInWorker,
            ::connectAccount,
        )
        sendMicrophone = CheckBox(this).apply {
            text = "Send my microphone to the host"
            setOnCheckedChangeListener { _, on ->
                if (!on) stopMicrophone()
                else if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
                    requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), RECORD_REQUEST)
                } else startMicrophone()
            }
        }
        launchLine = EditText(this).apply {
            hint = "Start on the host: program and arguments"
            inputType = InputType.TYPE_CLASS_TEXT
        }
        val refresh = Button(this).apply {
            text = "Refresh the window list"
            setOnClickListener {
                val s = session
                if (s == null) status.text = "Not connected" else worker.execute { s.requestWindows() }
            }
        }
        val shell = Button(this).apply {
            text = "Open a shell on the host"
            setOnClickListener { openHostTerminal() }
        }
        val launch = Button(this).apply {
            text = "Start on the host"
            setOnClickListener { launchOnHost(launchLine.text.toString()) }
        }
        sshUser = EditText(this).apply { hint = "SSH user"; inputType = InputType.TYPE_CLASS_TEXT }
        sshHost = EditText(this).apply {
            hint = "SSH server"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
        }
        sshPort = EditText(this).apply { hint = "port (22)"; inputType = InputType.TYPE_CLASS_NUMBER }
        sshPassword = EditText(this).apply {
            hint = "password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        sshButton = Button(this).apply {
            text = "Log in over SSH"
            setOnClickListener { sshLogin() }
        }
        terminalView = TerminalView(this).apply {
            visibility = View.GONE
            onEnded = { code -> status.text = "The shell ended (exit code $code). Press Back to close it." }
            onClipboard = { text ->
                fromHost = text
                clipboard.setPrimaryClip(ClipData.newPlainText("windowcast", text))
            }
        }
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(address, MATCH_PARENT, WRAP_CONTENT)
            addView(pin, MATCH_PARENT, WRAP_CONTENT)
            addView(connect, MATCH_PARENT, WRAP_CONTENT)
            addView(signInButton, MATCH_PARENT, WRAP_CONTENT)
            addView(cancelBrowserSignIn, MATCH_PARENT, WRAP_CONTENT)
            addView(sendMicrophone, MATCH_PARENT, WRAP_CONTENT)
            addView(status, MATCH_PARENT, WRAP_CONTENT)
            addView(refresh, MATCH_PARENT, WRAP_CONTENT)
            addView(shell, MATCH_PARENT, WRAP_CONTENT)
            addView(launchLine, MATCH_PARENT, WRAP_CONTENT)
            addView(launch, MATCH_PARENT, WRAP_CONTENT)
            addView(sshUser, MATCH_PARENT, WRAP_CONTENT)
            addView(sshHost, MATCH_PARENT, WRAP_CONTENT)
            addView(sshPort, MATCH_PARENT, WRAP_CONTENT)
            addView(sshPassword, MATCH_PARENT, WRAP_CONTENT)
            addView(sshButton, MATCH_PARENT, WRAP_CONTENT)
            addView(list, MATCH_PARENT, WRAP_CONTENT)
        }
        // The whole form scrolls, so a list of windows is never squeezed into what the form leaves.
        form = ScrollView(this).apply { addView(column, MATCH_PARENT, WRAP_CONTENT) }
        surface = StreamSurfaceView(this) { send(Input.text(it)) }.apply {
            visibility = View.GONE
            setOnTouchListener { view, event -> touch(view, event) }
        }
        spare = StreamSurfaceView(this) { send(Input.text(it)) }.apply {
            visibility = View.GONE
            setOnTouchListener { view, event -> touch(view, event) }
        }
        watchSurface(surface)
        watchSurface(spare)
        pointer = ImageView(this).apply {
            visibility = View.GONE
            isClickable = false
            isFocusable = false
            scaleType = ImageView.ScaleType.FIT_XY
        }
        clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
            val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString()
            val s = session
            if (text != null && text != fromHost && s != null) inputWorker.execute { s.setClipboard(text) }
        }.also { clipboard.addPrimaryClipChangedListener(it) }
        stage = FrameLayout(this).apply {
            addView(form, MATCH_PARENT, MATCH_PARENT)
            addView(surface, MATCH_PARENT, MATCH_PARENT)
            addView(spare, MATCH_PARENT, MATCH_PARENT)
            addView(pointer, WRAP_CONTENT, WRAP_CONTENT)
            addView(terminalView, MATCH_PARENT, MATCH_PARENT)
        }
        // A new screen size (rotation, split screen) changes the room the picture is fitted in.
        stage.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> fitSurfaces() }
        setContentView(stage)

        val kept = lastNonConfigurationInstance as? Kept
        if (kept != null) {
            // A recreation (a configuration change the manifest does not absorb): the
            // connection and the shell carry on, only the screens are rebuilt.
            client = kept.client
            val s = kept.session
            if (s != null && s.isOpen) {
                status.text = "Reconnected to the open session."
                sessions.adopt(s, sessionListener())
                worker.execute { s.requestWindows() }
                kept.terminal?.let { showTerminal(it) }
            } else {
                status.text = "Not connected"
            }
        } else {
            client = WindowcastClient(File(filesDir, "windowcast"))
            status.text = "This device: ${client?.peerId?.take(16)}…"
            // Not again after the system recreated the activity from saved state.
            if (savedInstanceState == null) open(intent)
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        open(intent)
    }

    /**
     * Another app's request to open a host's stream ([ConnectRequest], docs/ANDROID-VIEWER.md):
     * resume with that host at that address, then watch the window it names. It never pairs:
     * connecting without a PIN resumes only with a host in this viewer's trust store, and the
     * host that answers must be the one the request names. Anything but a request is ignored.
     */
    private fun open(intent: Intent?) {
        val request = when (intent?.action) {
            ConnectRequest.ACTION -> ConnectRequest.of(
                intent.getStringExtra(ConnectRequest.EXTRA_HOST_ID),
                intent.getStringExtra(ConnectRequest.EXTRA_ADDRESS),
                intent.getStringExtra(ConnectRequest.EXTRA_WINDOW),
            )
            Intent.ACTION_VIEW -> intent.dataString?.let { ConnectRequest.fromLink(it) }
            else -> return
        }
        if (request == null) {
            status.text = "Another app asked to open a host, but did not give its device id and address (HOST:PORT)."
            return
        }
        val current = session
        if (current != null && current.isOpen && current.hostId == request.hostId) {
            wanted = request.window?.let { WantedWindow(it, System.currentTimeMillis()) }
            worker.execute { current.requestWindows() }
            return
        }
        if (current != null && current.isOpen) {
            AlertDialog.Builder(this)
                .setTitle("Switch host?")
                .setMessage(
                    "Another app wants to open ${request.address}, but this viewer is still " +
                        "connected to a different host. Opening it closes that connection.",
                )
                .setPositiveButton("Switch") { _, _ -> openRequest(request) }
                .setNegativeButton("Stay connected") { _, _ -> }
                .show()
            return
        }
        openRequest(request)
    }

    /** Opens [request]'s host, replacing whatever session is current. Only called once the
     *  caller has either confirmed the switch with the user or there was nothing to switch away
     *  from. */
    private fun openRequest(request: ConnectRequest) {
        address.setText(request.address)
        wanted = request.window?.let { WantedWindow(it, System.currentTimeMillis()) }
        startSession(
            "Opening ${request.address}…",
            "Could not open ${request.address}",
            describe = { e -> FailureText.linkOpen(request.address, e.message) },
        ) { c ->
            val s = c.connect(request.address, null)
            if (s.hostId != request.hostId) {
                s.close()
                throw IllegalStateException(
                    "it answered as another computer (${s.hostId.take(16)}…), not the one asked for",
                )
            }
            s to { opened -> "Connected to " + opened.hostId.take(16) + "…" }
        }
    }

    /** What survives the activity being recreated: the connection and the shell, not the stream. */
    private class Kept(val client: WindowcastClient?, val session: WindowcastSession?, val terminal: TerminalSession?)

    @Deprecated("Kept across a recreation")
    override fun onRetainNonConfigurationInstance(): Any? {
        keepSession = true
        return Kept(client, session, terminalSession)
    }

    /** Back from the browser: if the sign-in there did not finish, say what is happening and how to stop it. */
    override fun onResume() {
        super.onResume()
        signIns.resumed()
    }

    private fun connect(address: String, pin: String?) {
        startSession("Connecting to $address…", "Could not connect") { c ->
            c.connect(address, pin) to { s -> (if (s.paired) "Paired with " else "Connected to ") + s.hostId.take(16) + "…" }
        }
    }

    /**
     * The one way a session starts, for a PIN connect and an account sign-in alike: the old
     * reader is stopped and joined before the old session closes (it may be inside nextEvent
     * on it, Droidtop/tracker#447), then [open] makes the new session and the text for its
     * status line.
     */
    private fun startSession(
        progress: String,
        failure: String,
        describe: ((Exception) -> String)? = null,
        open: (WindowcastClient) -> Pair<WindowcastSession, (WindowcastSession) -> String>,
    ) {
        val c = client ?: return
        status.text = progress
        // Windows watched beside the others belong to the session being replaced.
        closeStreamWindows(notifyHost = false)
        worker.execute {
            try {
                lateinit var done: (WindowcastSession) -> String
                sessions.start(
                    sessionListener(),
                    beforeClose = { stopMicrophone() },
                    opened = { s ->
                        main.post {
                            status.text = done(s)
                            if (sendMicrophone.isChecked) startMicrophone()
                        }
                    },
                ) {
                    val (opened, said) = open(c)
                    done = said
                    opened
                }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "connect failed", e)
                main.post { status.text = describe?.invoke(e) ?: "$failure: ${e.message}" }
            }
        }
    }

    private fun connectAccount(address: String, signIn: SignIn, accept: String?) {
        startSession("Signing in to $address…", "Could not sign in") { c ->
            c.connectAccount(address, signIn, accept) to { s -> "Signed in to " + s.hostId.take(16) + "…" }
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (requestCode != RECORD_REQUEST) return
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) startMicrophone()
        else {
            sendMicrophone.isChecked = false
            status.text = "The microphone needs permission to record"
        }
    }

    /** Sends the microphone over the current session, if there is one. */
    private fun startMicrophone() {
        val s = session ?: return
        if (microphone != null) return
        microphone = Microphone(s).also { it.start() }
    }

    /** Stops sending; waits for the sender (at most one 20 ms frame) so the session can close after. */
    private fun stopMicrophone() {
        val m = microphone ?: return
        microphone = null
        m.stop()
    }

    /** What this client does with each event, on the main thread; the library's [SessionHolder] delivers them through its [EventPump]. */
    private fun sessionListener(): SessionListener {
        return object : SessionListener {
            override fun onWindows(windows: List<WindowInfo>) {
                main.post { showWindows(windows) }
            }

            override fun onStreamStarted(event: Event.StreamStarted) {
                main.post {
                    // The window being watched, or one it owns that the
                    // library opened as it appeared.
                    if (event.window == watching?.id) {
                        startDecoding(event)
                    } else if (watchSet.isExtra(event.window)) {
                        openStreamWindow(event)
                    } else {
                        openPopup(event)
                    }
                }
            }

            override fun onCarrierStarted(event: Event.CarrierStarted) {
                main.post {
                    if (event.window == watching?.id) switchCarrier(event)
                }
            }

            override fun onCarrierRefused(event: Event.CarrierRefused) {
                main.post {
                    if (event.window == watching?.id && switcher.isPending(event.generation)) {
                        switching?.let { switcher.refuse(event.window, it.backend) }
                        cancelSwitch()
                        tell("No switch: ${event.reason}")
                    }
                }
            }

            override fun onStreamRefused(event: Event.StreamRefused) {
                main.post {
                    android.util.Log.i(TAG, "window ${event.window} refused: ${event.reason}")
                    if (event.window == watching?.id) {
                        status.text = "Refused: ${event.reason}"
                        showForm()
                    } else if (watchSet.isExtra(event.window)) {
                        watchSet.extraEnded(event.window)
                        tell("Refused: ${event.reason}")
                    } else {
                        // A window the host opened as it appeared (a
                        // dialog such as Save As) that could not be shown.
                        tell("A new window could not be shown: ${event.reason}")
                    }
                }
            }

            override fun onStreamStopped(event: Event.StreamStopped) {
                main.post {
                    if (popups.containsKey(event.window)) closePopup(event.window)
                    else if (extras.containsKey(event.window)) closeStreamWindow(event.window, notifyHost = false)
                    else if (event.window == watching?.id) stopDecoding()
                }
            }

            override fun onCursor(event: Event.Cursor) {
                main.post { if (event.window == watching?.id) showPointer(event) }
            }

            override fun onWindowResized(event: Event.WindowResized) {
                main.post { onResized(event) }
            }

            override fun onHostState(event: Event.HostState) {
                main.post {
                    val wasLocked = hostLocked
                    hostLocked = event.locked
                    if (wasLocked != event.locked) {
                        tell(
                            if (event.locked) {
                                "This PC is locked; its windows keep streaming. Input reaches them as window " +
                                    "messages, which some programs, most games among them, ignore."
                            } else {
                                "This PC is unlocked."
                            },
                        )
                    }
                    if (hostPrivate != event.privateSession) {
                        hostPrivate = event.privateSession
                        invalidateOptionsMenu()
                        when (event.privateSession) {
                            "active" -> tell("Private session: the PC's programs run out of sight of its screen.")
                            "available" -> android.util.Log.i(TAG, "the host offers a private session")
                            "unavailable" -> android.util.Log.i(TAG, "no private session: ${event.privateSessionReason}")
                        }
                    }
                }
            }

            override fun onCompatibility(event: Event.Compatibility) {
                main.post {
                    if (event.window == watching?.id) showCompatibility(event)
                }
            }

            override fun onPrivateSessionRefused(event: Event.PrivateSessionRefused) {
                main.post {
                    if (event.passwordNeeded) askPrivatePassword(event.reason) else tell("No private session: ${event.reason}")
                }
            }

            override fun onStreamQuality(event: Event.StreamQuality) {
                main.post {
                    val watched = watching ?: return@post
                    if (event.window == watched.id) {
                        qualityLine = event.quality.describe()
                        if (surface.visibility == View.VISIBLE) showStreamTitle(watched)
                    }
                }
            }

            override fun onClipboard(text: String) {
                main.post {
                    fromHost = text
                    clipboard.setPrimaryClip(ClipData.newPlainText("windowcast", text))
                }
            }

            override fun onClosed() {
                main.post {
                    status.text = "Disconnected"
                    stopMicrophone()
                    closeStreamWindows(notifyHost = false)
                    stopDecoding()
                }
            }
        }
    }

    /**
     * Says [text] on the status line, and as a toast while a stream covers that line, so a
     * refused switch or popup is not silent while watching (Droidtop/tracker#471).
     */
    private fun tell(text: String) {
        status.text = text
        if (streamingAny()) android.widget.Toast.makeText(this, text, android.widget.Toast.LENGTH_LONG).show()
    }

    private fun showWindows(list: List<WindowInfo>) {
        android.util.Log.i(TAG, "window list: ${list.size} windows")
        windows = list
        this.list.removeAllViews()
        for (window in list) {
            // The agent lists a window it cannot stream as 0x0.
            val capturable = window.width > 0 && window.height > 0
            this.list.addView(TextView(this).apply {
                text = "${window.title}  (${window.appId}, " + (if (capturable) "${window.width}x${window.height})" else "can't capture)")
                textSize = 18f
                setPadding(16, 24, 16, 24)
                isFocusable = true
                isClickable = true
                setOnClickListener {
                    if (capturable) watch(window) else status.text = "${window.title} can't be captured on this host"
                }
            }, MATCH_PARENT, WRAP_CONTENT)
        }
        status.text = launchNote.windowsLine(list.size, System.currentTimeMillis())
        watchWanted(list)
    }

    /** Watches the window another app asked for once it is listed; asks again while it is not ([WantedWindow] decides). */
    private fun watchWanted(list: List<WindowInfo>) {
        val want = wanted ?: return
        when (val step = want.next(list, System.currentTimeMillis())) {
            is WantedWindow.Step.Found -> {
                wanted = null
                watch(step.window)
            }
            WantedWindow.Step.GiveUp -> {
                wanted = null
                status.text = "No window titled \u201c${want.title}\u201d appeared. Tap one to watch it."
            }
            is WantedWindow.Step.Wait -> {
                status.text = "Waiting for a window titled \u201c${want.title}\u201d…"
                main.postDelayed({
                    val s = session
                    if (wanted === want && s != null && s.isOpen) worker.execute { s.requestWindows() }
                }, step.retryInMs)
            }
        }
    }

    private fun watch(window: WindowInfo) {
        val s = session ?: return
        when (watchSet.begin(window.id)) {
            // Asking again for a window already showing does nothing (it used to put a second
            // decoder on the same surface).
            WatchSet.Plan.ALREADY_SHOWING -> {
                tell("${window.title} is already on screen")
                return
            }
            WatchSet.Plan.PRIMARY -> {
                watching = window
                status.text = "Starting ${window.title}…"
            }
            WatchSet.Plan.EXTRA -> tell("Starting ${window.title} beside the others…")
        }
        val codecs = WindowDecoder.decodableCodecs()
        worker.execute { s.startWindow(window.id, codecs) }
    }

    override fun onCreateOptionsMenu(menu: Menu): Boolean {
        menu.add(Menu.NONE, MENU_VIDEO, Menu.NONE, "Show as video")
        menu.add(Menu.NONE, MENU_PICTURES, Menu.NONE, "Show as RDP pictures")
        menu.add(Menu.NONE, MENU_COMPATIBILITY, Menu.NONE, "Compatibility mode")
            .setCheckable(true)
        menu.add(Menu.NONE, MENU_KEYBOARD, Menu.NONE, "Keyboard")
        menu.add(Menu.NONE, MENU_ANOTHER, Menu.NONE, "Watch another window")
        menu.add(Menu.NONE, MENU_PRIVATE_START, Menu.NONE, "Use a private session")
        menu.add(Menu.NONE, MENU_PRIVATE_END, Menu.NONE, "End the private session")
        return true
    }

    override fun onPrepareOptionsMenu(menu: Menu): Boolean {
        val on = streaming() && switching == null
        val window = watching?.id
        fun offered(backend: String) = switcher.offered(window, backend)
        menu.findItem(MENU_VIDEO)?.isVisible = on && carrier != "Native" && offered("Native")
        menu.findItem(MENU_PICTURES)?.isVisible = on && carrier != "Rdp" && offered("Rdp")
        menu.findItem(MENU_COMPATIBILITY)?.let {
            it.isVisible = streaming()
            it.isChecked = compatibility
        }
        val connected = session?.isOpen == true
        menu.findItem(MENU_KEYBOARD)?.isVisible = streaming()
        menu.findItem(MENU_ANOTHER)?.isVisible = connected && streaming() && windows.isNotEmpty()
        menu.findItem(MENU_PRIVATE_START)?.isVisible = connected && hostPrivate == "available"
        menu.findItem(MENU_PRIVATE_END)?.isVisible = connected && hostPrivate == "active"
        return true
    }

    /** Switches the watched window's carrier, keeping it on screen until the new one shows. */
    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        when (item.itemId) {
            MENU_KEYBOARD -> {
                showKeyboard(surface)
                return true
            }
            MENU_ANOTHER -> {
                chooseAnotherWindow()
                return true
            }
            MENU_COMPATIBILITY -> {
                val s = session ?: return true
                val window = watching ?: return true
                val on = !compatibility
                worker.execute { s.setCompatibility(window.id, on) }
                return true
            }
            MENU_PRIVATE_START -> {
                AlertDialog.Builder(this)
                    .setTitle("Use a private session?")
                    .setMessage(
                        "Your programs on the PC keep running where nobody at it sees them, unlocked, " +
                            "until you end it. Someone signing in at the PC takes them back.",
                    )
                    .setPositiveButton("Start") { _, _ -> privateSession(true, null) }
                    .setNegativeButton("Cancel", null)
                    .show()
                return true
            }
            MENU_PRIVATE_END -> {
                privateSession(false, null)
                return true
            }
        }
        val backend = when (item.itemId) {
            MENU_VIDEO -> "Native"
            MENU_PICTURES -> "Rdp"
            else -> return super.onOptionsItemSelected(item)
        }
        val s = session ?: return true
        val window = watching ?: return true
        val codecs = WindowDecoder.decodableCodecs()
        worker.execute {
            try {
                s.switchWindow(window.id, backend, codecs)
            } catch (e: IOException) {
                android.util.Log.w(TAG, "switch window failed", e)
                main.post { status.text = "No switch: ${e.message}" }
            }
        }
        return true
    }

    private fun startDecoding(event: Event.StreamStarted) {
        carrier = event.backend
        // The switch items depend on the stream: build the menu again (#460).
        invalidateOptionsMenu()
        val window = watching ?: return
        val s = session ?: return
        form.visibility = View.GONE
        // Black bars around a picture that does not match the screen's shape.
        stage.setBackgroundColor(android.graphics.Color.BLACK)
        fitSurfaces()
        surface.visibility = View.VISIBLE
        val view = surface
        val begin = {
            // Its callbacks act only while it is the stream's renderer: a carrier switch stops
            // it, and its end must not end the stream now on the new carrier (#463).
            var mine: WindowRenderer? = null
            val report = { line: String, ended: Boolean, count: Long, error: String? ->
                main.post {
                    if (decoder === mine) {
                        if (surface.visibility == View.VISIBLE) { decoderLine = line; showStreamTitle(window) }
                        if (ended) {
                            status.text = "Stream ended after $count pictures" + (error?.let { ": $it" } ?: "")
                            stopDecoding()
                        }
                    }
                }
            }
            mine = WindowRenderers.create(s, window, event.backend, event.codec, view.holder) { p ->
                report(if (p.pictures) p.line else p.line + ", sound $soundPackets packets", p.ended, p.count, p.error)
            }.also {
                drawing[view] = it
                it.start()
            }
            decoder = mine
            soundPackets = 0
            audio = AudioPlayer(s, event.window) { packets -> soundPackets = packets }.also { it.start() }
        }
        if (surface.holder.surface?.isValid == true) {
            begin()
        } else {
            surface.holder.addCallback(object : SurfaceHolder.Callback {
                override fun surfaceCreated(holder: SurfaceHolder) {
                    holder.removeCallback(this)
                    begin()
                }
                override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
                override fun surfaceDestroyed(holder: SurfaceHolder) {}
            })
        }
    }

    /** A shell on the connected host. */
    private fun openHostTerminal() {
        val s = session ?: run { status.text = "Connect to a host first"; return }
        val (cols, rows) = terminalView.cellsFor(form.width, form.height)
        status.text = "Opening a shell…"
        worker.execute {
            try {
                val t = s.openTerminal(cols, rows)
                main.post { showTerminal(t) }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "open shell failed", e)
                main.post { status.text = "No shell: ${e.message}" }
            }
        }
    }

    /**
     * Starts an application on the connected host; its windows arrive in the list. A program the
     * host runs as a RemoteApp may need the user's Windows password: it is asked for and the
     * launch tried again with it ([password]), and kept nowhere.
     */
    private fun launchOnHost(line: String, password: String? = null) {
        val s = session ?: run { status.text = "Connect to a host first"; return }
        val argv = line.trim().split(" ").filter { it.isNotEmpty() }
        if (argv.isEmpty()) {
            status.text = "Type a program to start"
            return
        }
        status.text = "Starting ${argv[0]}..."
        worker.execute {
            try {
                val pid = s.launch(argv, password)
                s.requestWindows()
                main.post {
                    val note = "Started ${argv[0]}" + if (pid > 0) " (process $pid)" else ""
                    launchNote.set(note, System.currentTimeMillis())
                    status.text = note
                }
            } catch (e: dev.windowcast.PasswordNeededException) {
                main.post { askWindowsPassword(line, e.user) }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "launch failed", e)
                main.post { status.text = "Could not start it: ${e.message}" }
            }
        }
    }

    /**
     * The watched window's compatibility mode changed: how other programs (Remote Desktop, Moonlight,
     * VNC viewers) reach it, shown so they can be typed in, or why it was refused.
     */
    private fun showCompatibility(event: Event.Compatibility) {
        compatibility = event.endpoints.isNotEmpty()
        invalidateOptionsMenu()
        val refused = event.refused
        when {
            refused != null -> tell("No compatibility mode: $refused")
            event.endpoints.isEmpty() -> tell("Compatibility mode is off.")
            else -> AlertDialog.Builder(this)
                .setTitle("Compatibility mode")
                .setMessage(
                    "While you stream this window, other programs can show it too:\n\n" +
                        event.endpoints.joinToString("\n\n"),
                )
                .setPositiveButton("OK", null)
                .show()
        }
    }

    /** Starts or ends the host's private session, off the main thread. */
    private fun privateSession(start: Boolean, password: String?) {
        val s = session ?: return
        worker.execute { s.privateSession(start, password) }
    }

    /** The host needs its user's Windows password to start a private session. */
    private fun askPrivatePassword(reason: String) {
        val field = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            hint = "Windows password"
        }
        AlertDialog.Builder(this)
            .setTitle("Windows password")
            .setMessage(reason)
            .setView(field)
            .setPositiveButton("Start") { _, _ -> privateSession(true, field.text.toString()) }
            .setNegativeButton("Cancel") { _, _ -> status.text = "No private session" }
            .show()
    }

    /** Asks for [user]'s Windows password for a RemoteApp launch of [line]. */
    private fun askWindowsPassword(line: String, user: String) {
        val field = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            hint = "Windows password"
        }
        AlertDialog.Builder(this)
            .setTitle("Windows password for $user")
            .setMessage("The host runs this program through its Remote Desktop, which needs it.")
            .setView(field)
            .setPositiveButton("Start") { _, _ -> launchOnHost(line, field.text.toString()) }
            .setNegativeButton("Cancel") { _, _ -> status.text = "Not started" }
            .show()
    }

    /**
     * Logs in to an SSH server. A server not seen before is not trusted silently: its key is
     * shown, and pressing the button again trusts that key.
     */
    private fun sshLogin() {
        val c = client ?: return
        val host = sshHost.text.toString().trim()
        val user = sshUser.text.toString().trim()
        val port = sshPort.text.toString().trim().toIntOrNull() ?: 22
        val password = sshPassword.text.toString()
        if (host.isEmpty() || user.isEmpty()) {
            status.text = "An SSH server and a user are needed"
            return
        }
        val trust = pendingFingerprint
        val (cols, rows) = terminalView.cellsFor(form.width, form.height)
        status.text = "Logging in to $host…"
        worker.execute {
            try {
                val t = c.sshTerminal(
                    host, port, user, password,
                    policy = if (trust != null) HostKeyPolicy.FINGERPRINT else HostKeyPolicy.PINNED,
                    fingerprint = trust, cols = cols, rows = rows,
                )
                main.post {
                    pendingFingerprint = null
                    sshButton.text = "Log in over SSH"
                    showTerminal(t)
                }
            } catch (e: UntrustedHostKey) {
                main.post {
                    pendingFingerprint = e.fingerprint
                    sshButton.text = "Trust this key and log in"
                    status.text = "$host is not known yet. Its key is ${e.fingerprint}. Press the button to trust it."
                }
            } catch (e: IOException) {
                android.util.Log.w(TAG, "ssh login failed", e)
                main.post { status.text = "Could not log in: ${e.message}" }
            }
        }
    }

    private fun showTerminal(t: TerminalSession) {
        terminalSession?.let { old -> worker.execute { old.close() } }
        terminalSession = t
        form.visibility = View.GONE
        terminalView.visibility = View.VISIBLE
        terminalView.attach(t)
    }

    private fun closeTerminal() {
        terminalView.detach()
        terminalView.visibility = View.GONE
        form.visibility = View.VISIBLE
        showIdleStatus()
        val t = terminalSession
        terminalSession = null
        if (t != null) worker.execute { t.close() }
    }

    private fun streaming(): Boolean = surface.visibility == View.VISIBLE && watching != null

    /** Whether any window is showing, filling the screen or beside it. */
    private fun streamingAny(): Boolean = streaming() || extras.isNotEmpty()

    private fun send(input: Input) {
        val s = session ?: return
        inputWorker.execute { s.send(input) }
    }

    /** Every finger, as touches on the streamed window. */
    private fun touch(view: View, event: MotionEvent): Boolean {
        val window = watching?.id ?: return false
        return touchWindow(window, view, event)
    }

    /** Every finger on [view], as touches on [window]. */
    private fun touchWindow(window: Long, view: View, event: MotionEvent): Boolean {
        for (input in InputAdapter.touches(window, view, event)) send(input)
        return true
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (terminalView.visibility == View.VISIBLE && event.keyCode != KeyEvent.KEYCODE_BACK && terminalView.handleKey(event)) {
            return true
        }
        if (!streamingAny() || event.keyCode == KeyEvent.KEYCODE_BACK) return super.dispatchKeyEvent(event)
        return streamKey(event) || super.dispatchKeyEvent(event)
    }

    /**
     * Sends a key to the host as a hardware keyboard's would go: a key it has a code for as that
     * key, any other character as text. False if the key is not one to send.
     */
    private fun streamKey(event: KeyEvent): Boolean {
        if (gamepads.onKey(event)) return true
        val (input, handled) = InputAdapter.key(event)
        input?.let { send(it) }
        return handled
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streaming() && gamepads.onMotion(event)) return true
        return super.dispatchGenericMotionEvent(event)
    }

    /**
     * Shows a window the watched one owns (a dialog, popup or menu) in a floating window of
     * its own, at its own size as far as the screen allows; taps on it go to it.
     */
    private fun openPopup(event: Event.StreamStarted) {
        val s = session ?: return
        val info = windows.firstOrNull { it.id == event.window } ?: return
        if (popups.containsKey(event.window)) return
        val view = SurfaceView(this)
        val dialog = Dialog(this)
        val popup = PopupView(dialog, null)
        popups[event.window] = popup
        dialog.setTitle("${info.title} (${info.kind})")
        dialog.setContentView(view)
        sizeStreamWindow(dialog, info.width, info.height)
        placeAgainstOwner(dialog, info)
        view.setOnTouchListener { v, e -> touchWindow(event.window, v, e) }
        dialog.setOnDismissListener {
            // Closed here (Back, a tap outside): the host stops sending it.
            if (popups.remove(event.window) != null) {
                popup.renderer?.let { r -> worker.execute { r.stop() } }
                worker.execute { s.stopWindow(event.window) }
            }
        }
        view.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                popup.renderer = WindowRenderers.create(s, info, event.backend, event.codec, holder).also { it.start() }
            }
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                popup.renderer?.let { r -> worker.execute { r.stop() } }
                popup.renderer = null
            }
        })
        dialog.show()
    }

    /**
     * Puts a popup's floating window where it is on the host against its owner, as the owner is
     * shown on screen now (the watched window's surface), when the host gives both positions.
     */
    private fun placeAgainstOwner(dialog: Dialog, info: WindowInfo) {
        val owner = windows.firstOrNull { it.id == info.owner } ?: return
        if (owner.id != watching?.id || surface.visibility != View.VISIBLE) return
        val origin = IntArray(2)
        surface.getLocationOnScreen(origin)
        val box = WindowLayout.againstOwner(info, owner, origin[0], origin[1], surface.width, surface.height) ?: return
        val attributes = dialog.window?.attributes ?: return
        attributes.gravity = android.view.Gravity.TOP or android.view.Gravity.START
        attributes.x = box.x
        attributes.y = box.y
        attributes.width = box.width
        attributes.height = box.height
        dialog.window?.attributes = attributes
    }

    /** The host stopped a popup (it closed): its floating window goes. */
    private fun closePopup(window: Long) {
        val popup = popups.remove(window) ?: return
        popup.renderer?.let { r -> worker.execute { r.stop() } }
        popup.dialog.dismiss()
    }

    /**
     * Opens a window watched beside the one that fills the screen as a floating window of its
     * own: its own surface, decoder and sound, with buttons to type into it and to close it. It
     * stays until the user closes it (Close, or Back), the host closes the window, or the
     * session ends. It is not closed when the full-screen window is.
     */
    private fun openStreamWindow(event: Event.StreamStarted) {
        val s = session ?: return
        val info = windows.firstOrNull { it.id == event.window }
        if (info == null) {
            watchSet.extraEnded(event.window)
            return
        }
        if (extras.containsKey(event.window)) return
        val view = StreamSurfaceView(this) { send(Input.text(it)) }
        view.setOnTouchListener { v, e -> touchWindow(event.window, v, e) }
        val dialog = object : Dialog(this@MainActivity) {
            // Keys typed while this window is up go to the host like any others; Back closes it.
            override fun dispatchKeyEvent(e: KeyEvent): Boolean =
                (e.keyCode != KeyEvent.KEYCODE_BACK && streamKey(e)) || super.dispatchKeyEvent(e)
        }
        val buttons = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(
                Button(this@MainActivity).apply {
                    text = "Keyboard"
                    setOnClickListener { showKeyboard(view) }
                },
                WRAP_CONTENT,
                WRAP_CONTENT,
            )
            addView(
                Button(this@MainActivity).apply {
                    text = "Close"
                    setOnClickListener { dialog.dismiss() }
                },
                WRAP_CONTENT,
                WRAP_CONTENT,
            )
        }
        val content = FrameLayout(this).apply {
            addView(view, MATCH_PARENT, MATCH_PARENT)
            addView(
                buttons,
                FrameLayout.LayoutParams(
                    WRAP_CONTENT,
                    WRAP_CONTENT,
                    android.view.Gravity.TOP or android.view.Gravity.END,
                ),
            )
        }
        val shown = StreamWindow(dialog, null, null)
        extras[event.window] = shown
        dialog.setTitle(info.title)
        dialog.setContentView(content)
        dialog.setCanceledOnTouchOutside(false)
        sizeStreamWindow(dialog, info.width, info.height)
        // The keyboard covers the bottom of the window instead of pushing it about.
        dialog.window?.setSoftInputMode(android.view.WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        dialog.setOnDismissListener {
            // Closed here (Close, Back): stop it and tell the host. A window the host closed, or
            // one closed with the session, left the map first, so this does nothing for them.
            if (extras.remove(event.window) != null) {
                watchSet.extraEnded(event.window)
                release(shown)
                worker.execute { s.stopWindow(event.window) }
            }
        }
        view.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                shown.renderer = WindowRenderers.create(s, info, event.backend, event.codec, holder).also { it.start() }
            }
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                shown.renderer?.let { r -> worker.execute { r.stop() } }
                shown.renderer = null
            }
        })
        shown.audio = AudioPlayer(s, event.window).also { it.start() }
        dialog.show()
    }

    /** Stops what a floating stream window is running; the window itself is not touched. */
    private fun release(shown: StreamWindow) {
        shown.renderer?.let { r -> worker.execute { r.stop() } }
        shown.renderer = null
        shown.audio?.let { a -> worker.execute { a.stop() } }
        shown.audio = null
    }

    /** Closes one floating stream window; with [notifyHost] the host is told to stop sending it. */
    private fun closeStreamWindow(window: Long, notifyHost: Boolean) {
        val shown = extras.remove(window) ?: return
        watchSet.extraEnded(window)
        release(shown)
        shown.dialog.dismiss()
        if (notifyHost) session?.let { s -> worker.execute { s.stopWindow(window) } }
    }

    /** Closes every floating stream window: the session they belong to is ending or gone. */
    private fun closeStreamWindows(notifyHost: Boolean) {
        for (window in extras.keys.toList()) closeStreamWindow(window, notifyHost)
        watchSet.clear()
    }

    /** Brings up the soft keyboard for [view], a stream's surface. */
    private fun showKeyboard(view: View) {
        view.requestFocus()
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as android.view.inputmethod.InputMethodManager
        imm.showSoftInput(view, 0)
    }

    /** Lists the host's windows not yet showing, and watches the one picked beside the others. */
    private fun chooseAnotherWindow() {
        val choices = windows.filter {
            it.owner == null && it.width > 0 && it.height > 0 && !watchSet.isWatched(it.id)
        }
        if (choices.isEmpty()) {
            tell("There is no other window to watch")
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Watch another window")
            .setItems(choices.map { "${it.title} (${it.appId})" }.toTypedArray()) { _, which ->
                watch(choices[which])
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /**
     * Shows a switch's new carrier on the spare surface, beside the one on screen, and swaps the
     * two on its first picture; then the old carrier's renderer stops and the library is told.
     */
    private fun switchCarrier(event: Event.CarrierStarted) {
        val s = session ?: return
        val window = watching ?: return
        cancelSwitch()
        val pending = switcher.begin(event.generation, event.backend)
        invalidateOptionsMenu()
        val view = spare
        view.visibility = View.VISIBLE
        val begin = { holder: SurfaceHolder ->
            var first = true
            val onShown = {
                if (first) {
                    first = false
                    main.post { finishSwitch(pending, view) }
                }
            }
            // Once swapped in, it reports as the first carrier's renderer did.
            val report = { line: String, ended: Boolean, error: String? ->
                main.post {
                    if (decoder === pending.renderer) {
                        if (surface.visibility == View.VISIBLE) { decoderLine = line; showStreamTitle(window) }
                        if (ended) {
                            status.text = "Stream ended" + (error?.let { ": $it" } ?: "")
                            stopDecoding()
                        }
                    }
                }
            }
            pending.renderer = WindowRenderers.create(s, window, event.backend, event.codec, holder) { p ->
                if (p.count > 0) onShown()
                report(p.line, p.ended, p.error)
            }.also {
                drawing[view] = it
                it.start()
            }
        }
        if (view.holder.surface?.isValid == true) {
            begin(view.holder)
        } else {
            view.holder.addCallback(object : SurfaceHolder.Callback {
                override fun surfaceCreated(holder: SurfaceHolder) {
                    holder.removeCallback(this)
                    if (switching === pending) begin(holder)
                }
                override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}
                override fun surfaceDestroyed(holder: SurfaceHolder) {}
            })
        }
    }

    /** The new carrier showed its first picture: it takes the screen, the old one stops. */
    private fun finishSwitch(pending: CarrierSwitcher.Pending<WindowRenderer>, view: SurfaceView) {
        val s = session ?: return
        val window = watching ?: return
        if (!switcher.finish(pending)) return
        carrier = pending.backend
        invalidateOptionsMenu()
        decoder = pending.renderer
        val shown = surface
        surface = view
        spare = shown
        // Hiding it destroys its surface, which stops its renderer first.
        shown.visibility = View.GONE
        worker.execute { s.carrierShown(window.id, pending.generation) }
    }

    /** A switch that will not happen: its renderer stops and its surface goes. */
    private fun cancelSwitch() {
        val pending = switcher.cancel() ?: return
        spare.visibility = View.GONE
        // A renderer that never got its surface on screen stops here.
        pending.renderer?.let { r ->
            if (drawing.values.none { it === r }) worker.execute { r.stop() }
        }
        invalidateOptionsMenu()
    }

    private fun stopDecoding() {
        cancelSwitch()
        // A watched window's popups go with it.
        for (window in popups.keys.toList()) closePopup(window)
        gamepads.releaseAll()
        decoder?.let { d -> worker.execute { d.stop() } }
        decoder = null
        audio?.let { a -> worker.execute { a.stop() } }
        audio = null
        showForm()
    }

    /**
     * Sizes the stream surfaces to the watched window's shape, centred in the frame, so the
     * picture is not stretched to the screen. Both surfaces, because a carrier switch shows the
     * new one beside the old and swaps them.
     */
    private fun fitSurfaces() {
        val window = watching
        val fit = if (window == null) null else Letterbox.fit(window.width, window.height, stage.width, stage.height)
        val width = fit?.first ?: MATCH_PARENT
        val height = fit?.second ?: MATCH_PARENT
        for (view in listOf<View>(surface, spare)) {
            val params = view.layoutParams as FrameLayout.LayoutParams
            if (params.width != width || params.height != height || params.gravity != android.view.Gravity.CENTER) {
                params.width = width
                params.height = height
                params.gravity = android.view.Gravity.CENTER
                view.layoutParams = params
            }
        }
    }

    /** The host resized a window: refit the picture, or the floating window, to its new shape. */
    private fun onResized(event: Event.WindowResized) {
        if (event.width <= 0 || event.height <= 0) return
        val window = watching
        if (window != null && window.id == event.window) {
            watching = window.copy(width = event.width, height = event.height)
            if (streaming()) {
                fitSurfaces()
                lastCursor?.let { showPointer(it) }
            }
        }
        extras[event.window]?.let { sizeStreamWindow(it.dialog, event.width, event.height) }
    }

    /** Sizes a floating stream window to a [width] by [height] picture, shrunk to fit the screen. */
    private fun sizeStreamWindow(dialog: Dialog, width: Int, height: Int) {
        val screen = resources.displayMetrics
        val (w, h) = WindowLayout.floatingSize(width, height, screen.widthPixels, screen.heightPixels)
        dialog.window?.setLayout(w, h)
    }

    /** What the status line says when nothing is going on, so no old message outlives its screen. */
    private fun showIdleStatus() {
        status.text = if (session?.isOpen == true) "${windows.size} windows. Tap one to watch it." else "Not connected"
    }

    /**
     * Draws the host's pointer at [cursor] over the watched window: its shape scaled as the
     * window's picture is, its hotspot on the point. A shape not seen before is fetched off the
     * main thread first.
     */
    private fun showPointer(cursor: Event.Cursor) {
        lastCursor = cursor
        val window = watching
        if (!cursor.visible || !streaming() || window == null) {
            pointer.visibility = View.GONE
            return
        }
        val shape = pointerShapes[cursor.shape]
        if (shape == null) {
            val s = session ?: return
            worker.execute {
                val image = s.cursorImage(cursor.shape) ?: return@execute
                val bitmap = Bitmap.createBitmap(image.argb, image.width, image.height, Bitmap.Config.ARGB_8888)
                main.post {
                    pointerShapes[cursor.shape] = bitmap to (image.hotspotX to image.hotspotY)
                    lastCursor?.let { if (it.shape == cursor.shape) showPointer(it) }
                }
            }
            return
        }
        val (bitmap, hotspot) = shape
        val scale = surface.width.toFloat() / window.width.coerceAtLeast(1)
        val width = (bitmap.width * scale).toInt().coerceAtLeast(1)
        val height = (bitmap.height * scale).toInt().coerceAtLeast(1)
        if (pointer.drawable == null || (pointer.tag as? Long) != cursor.shape) {
            pointer.setImageBitmap(bitmap)
            pointer.tag = cursor.shape
        }
        val params = pointer.layoutParams
        if (params.width != width || params.height != height) {
            params.width = width
            params.height = height
            pointer.layoutParams = params
        }
        // The picture may sit inside bars, so its position in the frame counts.
        pointer.translationX = surface.left + cursor.x * surface.width - hotspot.first * scale
        pointer.translationY = surface.top + cursor.y * surface.height - hotspot.second * scale
        pointer.visibility = View.VISIBLE
    }

    private fun showForm() {
        // The full-screen stream is gone; windows watched beside it stay.
        watchSet.primaryEnded()
        compatibility = false
        pointer.visibility = View.GONE
        lastCursor = null
        surface.visibility = View.GONE
        stage.setBackgroundColor(android.graphics.Color.TRANSPARENT)
        form.visibility = View.VISIBLE
        // The stream's title (window name and counters) goes with the stream.
        decoderLine = ""
        qualityLine = null
        title = APP_TITLE
        invalidateOptionsMenu()
    }

    @Deprecated("Activity back handling")
    override fun onBackPressed() {
        if (terminalView.visibility == View.VISIBLE) {
            closeTerminal()
            return
        }
        val window = watching
        val s = session
        if (surface.visibility == View.VISIBLE && window != null && s != null) {
            worker.execute { s.stopWindow(window.id) }
            stopDecoding()
            showIdleStatus()
        } else {
            super.onBackPressed()
        }
    }

    override fun onDestroy() {
        val extraIds = extras.keys.toList()
        closeStreamWindows(notifyHost = false)
        clipboardListener?.let { clipboard.removePrimaryClipChangedListener(it) }
        inputWorker.shutdown()
        // Sign-in waits end with the activity, and nothing it posted runs after.
        signInWorker.shutdownNow()
        main.removeCallbacksAndMessages(null)
        terminalView.detach()
        decoder?.stop()
        audio?.stop()
        stopMicrophone()
        if (keepSession && isChangingConfigurations) {
            // Recreated, not finished: the next instance takes the session and the shell over.
            // The stream does not survive its surface, so the host is told to stop it.
            val s = sessions.release(wait = false)
            val w = watching
            if (decoder != null && s != null && w != null) worker.execute { s.stopWindow(w.id) }
            // Nor do the windows watched beside it.
            if (s != null) for (id in extraIds) worker.execute { s.stopWindow(id) }
            // What is queued still runs, then the worker ends.
            worker.shutdown()
        } else {
            // Work still queued or waiting on the network ends with the activity.
            worker.shutdownNow()
            terminalSession?.close()
            sessions.end()
            client?.close()
        }
        super.onDestroy()
    }

    companion object {
        private const val TAG = "windowcast"
        private const val RECORD_REQUEST = 1
        private const val MENU_VIDEO = 1
        private const val MENU_PICTURES = 2
        private const val MENU_PRIVATE_START = 3
        private const val MENU_PRIVATE_END = 4
        private const val MENU_COMPATIBILITY = 5
        private const val MENU_KEYBOARD = 6
        private const val MENU_ANOTHER = 7
        /** The activity's label in the manifest: the title when no stream is showing. */
        private const val APP_TITLE = "windowcast viewer"
    }
}
