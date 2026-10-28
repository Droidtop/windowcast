package dev.windowcast.viewer

import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.browser.customtabs.CustomTabsIntent
import dev.windowcast.OidcDeviceSignIn
import dev.windowcast.OidcProvider
import dev.windowcast.SignIn
import dev.windowcast.SignInOptions
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
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
import dev.windowcast.Codec
import dev.windowcast.Event
import dev.windowcast.WindowDecoder
import dev.windowcast.WindowPictures
import dev.windowcast.WindowRenderer
import dev.windowcast.WindowInfo
import dev.windowcast.WindowcastClient
import dev.windowcast.WindowcastSession
import dev.windowcast.Gamepads
import dev.windowcast.Input
import dev.windowcast.Keys
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
import java.util.concurrent.Executors

/**
 * A minimal windowcast client for trying the library against a host:
 * connect (PIN the first time), pick a window, watch it. Everything it
 * does goes through the windowcast Android library.
 */
class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
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

    /** A switch under way: the new carrier's generation and renderer. */
    private class Switch(val generation: Int, val backend: String, var renderer: WindowRenderer? = null)

    private var switching: Switch? = null

    /**
     * Switches a host has refused, as host:window:backend, so the menu stops offering one it
     * has already said it cannot serve (Droidtop/tracker#471). Per window, since a host can
     * refuse one window's switch and serve another's.
     */
    private val refusedSwitches = mutableSetOf<String>()

    private fun refusedKey(window: Long, backend: String): String =
        "${session?.hostId}:$window:$backend"

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
    private var clipboardListener: ClipboardManager.OnPrimaryClipChangedListener? = null
    /** Counts browser sign-ins; a finished one acts only if it is still the latest, so Cancel can drop one. */
    private val signInAttempt = AtomicInteger()
    @Volatile private var browserSignInWaiting = false
    /** Set once the system has asked for the state to keep across a recreation (onDestroy keeps the session then). */
    private var keepSession = false
    private lateinit var sendMicrophone: CheckBox
    @Volatile private var microphone: Microphone? = null

    private var client: WindowcastClient? = null
    @Volatile private var session: WindowcastSession? = null
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
    private var launchNote: String? = null
    private var watching: WindowInfo? = null

    /**
     * The window another app asked for ([ConnectRequest.window], part of its title), watched as
     * soon as the host lists it; asked again every [WANTED_REFRESH_MS] until [WANTED_WITHIN_MS].
     */
    private var wanted: String? = null
    private var wantedSince = 0L
    /** The thread reading the current session's events; replacing or clearing it stops the old one. */
    @Volatile private var listener: Thread? = null

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
                signIn(address.text.toString().trim())
            }
        }
        cancelBrowserSignIn = Button(this).apply {
            text = "Cancel sign-in"
            visibility = View.GONE
            setOnClickListener { giveUpBrowserSignIn("Sign-in cancelled") }
        }
        status = TextView(this).apply { text = "Not connected" }
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
            session = kept.session
            val s = kept.session
            if (s != null && s.isOpen) {
                status.text = "Reconnected to the open session."
                listen(s)
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
            wanted = request.window
            wantedSince = System.currentTimeMillis()
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
        wanted = request.window
        wantedSince = System.currentTimeMillis()
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
        if (browserSignInWaiting) {
            status.text = "Still waiting for the sign-in in the browser. Finish it there, or press Cancel sign-in."
        }
    }

    /** Gives up the browser sign-in that is waiting, if any; its result is ignored when it arrives or times out. */
    private fun giveUpBrowserSignIn(message: String) {
        signInAttempt.incrementAndGet()
        browserSignInWaiting = false
        cancelBrowserSignIn.visibility = View.GONE
        status.text = message
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
                stopMicrophone()
                stopListening()
                session?.close()
                session = null
                val (s, done) = open(c)
                // Windows the rules send to RDP (text) come as pictures.
                s.acceptPictures(true)
                session = s
                main.post {
                    status.text = done(s)
                    if (sendMicrophone.isChecked) startMicrophone()
                }
                listen(s)
                s.requestWindows()
            } catch (e: Exception) {
                main.post { status.text = describe?.invoke(e) ?: "$failure: ${e.message}" }
            }
        }
    }

    /**
     * Account sign-in (docs/ACCOUNTS.md): ask the host what it takes; a host not trusted yet
     * shows its fingerprint first, and nothing is sent to it until the user says it matches.
     */
    private fun signIn(address: String) {
        val c = client ?: return
        if (address.isEmpty()) {
            status.text = "Type the host's address first"
            return
        }
        status.text = "Asking $address how to sign in…"
        worker.execute {
            try {
                val options = c.signInOptions(address)
                main.post { confirmHost(address, options) }
            } catch (e: Exception) {
                main.post { status.text = "Could not ask the host: ${e.message}" }
            }
        }
    }

    private fun confirmHost(address: String, options: SignInOptions) {
        if (options.trusted) {
            chooseSignIn(address, options, null)
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Is this your host?")
            .setMessage(
                "This device has not signed in to this host before. Check that the host shows " +
                    "this fingerprint:\n\n${options.fingerprint}\n\nIf it shows another, cancel: " +
                    "a different machine is answering.",
            )
            .setPositiveButton("It matches") { _, _ -> chooseSignIn(address, options, options.hostId) }
            .setNegativeButton("Cancel") { _, _ -> status.text = "Sign-in cancelled" }
            .show()
    }

    /** The ways this host takes; [accept] is the host identity the user confirmed, if any. */
    private fun chooseSignIn(address: String, options: SignInOptions, accept: String?) {
        val choices = mutableListOf<Pair<String, () -> Unit>>()
        if (options.password) choices += "User name and password" to { askPassword(address, accept) }
        for (provider in options.providers) {
            choices += "${provider.name} in the browser" to { browserSignIn(address, provider, accept) }
            choices += "${provider.name} with a code on another device" to { deviceSignIn(address, provider, accept) }
        }
        if (choices.isEmpty()) {
            status.text = "This host takes no sign-in this app can do"
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Sign in")
            .setItems(choices.map { it.first }.toTypedArray()) { _, which -> choices[which].second() }
            .setNegativeButton("Cancel") { _, _ -> status.text = "Sign-in cancelled" }
            .show()
    }

    private fun askPassword(address: String, accept: String?) {
        val user = EditText(this).apply { hint = "User name"; inputType = InputType.TYPE_CLASS_TEXT }
        val password = EditText(this).apply {
            hint = "Password"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        val fields = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 16, 48, 0)
            addView(user, MATCH_PARENT, WRAP_CONTENT)
            addView(password, MATCH_PARENT, WRAP_CONTENT)
        }
        AlertDialog.Builder(this)
            .setTitle("Sign in with a password")
            .setView(fields)
            .setPositiveButton("Sign in") { _, _ ->
                connectAccount(address, SignIn.Password(user.text.toString().trim(), password.text.toString()), accept)
            }
            .setNegativeButton("Cancel") { _, _ -> status.text = "Sign-in cancelled" }
            .show()
    }

    /**
     * The provider's page in a Custom Tab. The provider sends the browser back to the library's
     * loopback port on this device, which hands over the ID token; then this activity comes back
     * to the front, closing the tab.
     */
    private fun browserSignIn(address: String, provider: OidcProvider, accept: String?) {
        val c = client ?: return
        val attempt = signInAttempt.incrementAndGet()
        browserSignInWaiting = true
        cancelBrowserSignIn.visibility = View.VISIBLE
        status.text = "Opening ${provider.name}…"
        Thread({
            try {
                val signIn = c.oidcBrowser(provider)
                main.post {
                    try {
                        CustomTabsIntent.Builder().build().launchUrl(this, Uri.parse(signIn.url))
                    } catch (e: Exception) {
                        status.text = "No browser to sign in with: ${e.message}"
                    }
                }
                val token = signIn.finish(SIGN_IN_TIMEOUT_MS)
                main.post {
                    // Cancelled meanwhile: the token is dropped.
                    if (signInAttempt.get() != attempt) return@post
                    browserSignInWaiting = false
                    cancelBrowserSignIn.visibility = View.GONE
                    backToFront()
                    connectAccount(address, SignIn.Oidc(provider, token), accept)
                }
            } catch (e: Exception) {
                main.post {
                    if (signInAttempt.get() != attempt) return@post
                    giveUpBrowserSignIn("Could not sign in with ${provider.name}: ${e.message}")
                }
            }
        }, "windowcast-sign-in").start()
    }

    /** The provider's code, entered on another device (or opened here); waits until the user is done. */
    private fun deviceSignIn(address: String, provider: OidcProvider, accept: String?) {
        val c = client ?: return
        status.text = "Asking ${provider.name} for a code…"
        Thread({
            val signIn = try {
                c.oidcDevice(provider)
            } catch (e: Exception) {
                main.post { status.text = "Could not sign in with ${provider.name}: ${e.message}" }
                null
            }
            if (signIn != null) awaitDeviceSignIn(address, provider, accept, signIn)
        }, "windowcast-sign-in").start()
    }

    /** Shows the code and waits (on the calling thread) until the user is done or cancels. */
    private fun awaitDeviceSignIn(address: String, provider: OidcProvider, accept: String?, signIn: OidcDeviceSignIn) {
        val cancelled = AtomicBoolean(false)
        val dialog = arrayOfNulls<AlertDialog>(1)
        main.post {
            dialog[0] = AlertDialog.Builder(this)
                .setTitle("Sign in with ${provider.name}")
                .setMessage("On another device, open\n\n${signIn.verificationUri}\n\nand enter the code\n\n${signIn.userCode}")
                .setNegativeButton("Cancel") { _, _ -> cancelled.set(true) }
                .setNeutralButton("Open it here") { _, _ ->
                    // The dialog closes; the wait goes on until the provider says.
                    val page = signIn.verificationUriComplete ?: signIn.verificationUri
                    CustomTabsIntent.Builder().build().launchUrl(this, Uri.parse(page))
                }
                .setCancelable(false)
                .show()
            status.text = "Waiting for the sign-in with ${provider.name}…"
        }
        try {
            var waited: String? = null
            while (waited == null && !cancelled.get()) waited = signIn.await(1000)
            val token = waited
            main.post {
                dialog[0]?.dismiss()
                if (token != null) {
                    backToFront()
                    connectAccount(address, SignIn.Oidc(provider, token), accept)
                } else {
                    status.text = "Sign-in cancelled"
                }
            }
        } catch (e: Exception) {
            main.post {
                dialog[0]?.dismiss()
                status.text = "Could not sign in with ${provider.name}: ${e.message}"
            }
        } finally {
            signIn.close()
        }
    }

    /** Brings this activity back over a Custom Tab opened from it, closing the tab. */
    private fun backToFront() {
        startActivity(
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        )
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

    /** Tells the reader to stop and, off the main thread, waits for it (it polls every 500 ms). */
    private fun stopListening(wait: Boolean = true) {
        val t = listener ?: return
        listener = null
        if (wait && t !== Thread.currentThread() && Looper.myLooper() != Looper.getMainLooper()) t.join(2000)
    }

    /** Reads session events on their own thread; it ends when it is no longer [listener] or [s] closes. */
    private fun listen(s: WindowcastSession) {
        val reader = Thread({
            val me = Thread.currentThread()
            while (listener === me && s.isOpen) {
                when (val event = s.nextEvent(500) ?: continue) {
                    is Event.Windows -> main.post { showWindows(event.windows) }
                    is Event.StreamStarted -> main.post {
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
                    is Event.CarrierStarted -> main.post {
                        if (event.window == watching?.id) switchCarrier(event)
                    }
                    is Event.CarrierRefused -> main.post {
                        if (event.window == watching?.id && switching?.generation == event.generation) {
                            switching?.let { refusedSwitches += refusedKey(event.window, it.backend) }
                            cancelSwitch()
                            tell("No switch: ${event.reason}")
                        }
                    }
                    is Event.StreamRefused -> main.post {
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
                    is Event.StreamStopped -> main.post {
                        if (popups.containsKey(event.window)) closePopup(event.window)
                        else if (extras.containsKey(event.window)) closeStreamWindow(event.window, notifyHost = false)
                        else if (event.window == watching?.id) stopDecoding()
                    }
                    is Event.Cursor -> main.post { if (event.window == watching?.id) showPointer(event) }
                    is Event.WindowResized -> main.post { onResized(event) }
                    is Event.HostState -> main.post {
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
                    is Event.Compatibility -> main.post {
                        if (event.window == watching?.id) showCompatibility(event)
                    }
                    is Event.PrivateSessionRefused -> main.post {
                        if (event.passwordNeeded) askPrivatePassword(event.reason) else tell("No private session: ${event.reason}")
                    }
                    is Event.Clipboard -> main.post {
                        fromHost = event.text
                        clipboard.setPrimaryClip(ClipData.newPlainText("windowcast", event.text))
                    }
                    is Event.Closed -> {
                        if (listener === me) {
                            main.post {
                                status.text = "Disconnected"
                                stopMicrophone()
                                closeStreamWindows(notifyHost = false)
                                stopDecoding()
                            }
                        }
                        break
                    }
                    else -> {}
                }
            }
        }, "windowcast-events")
        listener = reader
        reader.start()
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
        status.text = (launchNote?.let { "$it. " } ?: "") + "${list.size} windows. Tap one to watch it."
        launchNote = null
        watchWanted(list)
    }

    /** Watches the window another app asked for once it is listed; asks again while it is not. */
    private fun watchWanted(list: List<WindowInfo>) {
        val title = wanted ?: return
        val found = list.firstOrNull {
            it.width > 0 && it.height > 0 && it.title.contains(title, ignoreCase = true)
        }
        if (found != null) {
            wanted = null
            watch(found)
            return
        }
        if (System.currentTimeMillis() - wantedSince > WANTED_WITHIN_MS) {
            wanted = null
            status.text = "No window titled \u201c$title\u201d appeared. Tap one to watch it."
            return
        }
        status.text = "Waiting for a window titled \u201c$title\u201d…"
        main.postDelayed({
            val s = session
            if (wanted == title && s != null && s.isOpen) worker.execute { s.requestWindows() }
        }, WANTED_REFRESH_MS)
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
        fun offered(backend: String) =
            window == null || refusedKey(window, backend) !in refusedSwitches
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
            } catch (e: Exception) {
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
                        if (surface.visibility == View.VISIBLE) title = "${window.title}: $line"
                        if (ended) {
                            status.text = "Stream ended after $count pictures" + (error?.let { ": $it" } ?: "")
                            stopDecoding()
                        }
                    }
                }
            }
            mine = if (event.backend == "Rdp") {
                WindowPictures(s, event.window, view.holder) { stats ->
                    report(
                        "${stats.pictures} pictures over RDP, ${stats.width}x${stats.height}",
                        stats.ended, stats.pictures, stats.error,
                    )
                }
            } else {
                WindowDecoder(s, event.window, view.holder.surface, window.width, window.height) { stats ->
                    report(
                        "${stats.frames} frames (${stats.codec ?: event.codec}), sound $soundPackets packets",
                        stats.ended, stats.frames, stats.error,
                    )
                }
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
            } catch (e: Exception) {
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
                    launchNote = "Started ${argv[0]}" + if (pid > 0) " (process $pid)" else ""
                    status.text = launchNote
                    refreshWindowsSoon()
                }
            } catch (e: dev.windowcast.PasswordNeededException) {
                main.post { askWindowsPassword(line, e.user) }
            } catch (e: Exception) {
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

    /** The new window is not mapped when the launch returns: ask for the list again a few times. */
    private fun refreshWindowsSoon() {
        for (delay in REFRESH_DELAYS_MS) {
            main.postDelayed({ session?.let { s -> worker.execute { s.requestWindows() } } }, delay)
        }
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
            } catch (e: Exception) {
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
        fun put(index: Int, phase: Input.Touch) {
            val x = event.getX(index) / view.width.coerceAtLeast(1)
            val y = event.getY(index) / view.height.coerceAtLeast(1)
            send(Input.touch(window, event.getPointerId(index), x, y, phase))
        }
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> put(event.actionIndex, Input.Touch.Start)
            MotionEvent.ACTION_MOVE -> for (i in 0 until event.pointerCount) put(i, Input.Touch.Move)
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> put(event.actionIndex, Input.Touch.End)
            MotionEvent.ACTION_CANCEL -> for (i in 0 until event.pointerCount) put(i, Input.Touch.Cancel)
        }
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
        val down = event.action == KeyEvent.ACTION_DOWN
        val evdev = Keys.evdev(event.keyCode)
        return when {
            evdev != null -> {
                if (event.repeatCount == 0) send(Input.key(evdev, down))
                true
            }
            down && event.unicodeChar != 0 -> {
                send(Input.text(String(Character.toChars(event.unicodeChar))))
                true
            }
            else -> false
        }
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
        val screen = resources.displayMetrics
        val scale = minOf(
            1f,
            screen.widthPixels * 0.9f / info.width.coerceAtLeast(1),
            screen.heightPixels * 0.8f / info.height.coerceAtLeast(1),
        )
        dialog.window?.setLayout(
            (info.width * scale).toInt().coerceAtLeast(64),
            (info.height * scale).toInt().coerceAtLeast(64),
        )
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
                popup.renderer = (if (event.backend == "Rdp") {
                    WindowPictures(s, event.window, holder)
                } else {
                    WindowDecoder(s, event.window, holder.surface, info.width, info.height)
                }).also { it.start() }
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
        val (px, py) = info.position ?: return
        val (ox, oy) = owner.position ?: return
        val origin = IntArray(2)
        surface.getLocationOnScreen(origin)
        val sx = surface.width.toFloat() / owner.width.coerceAtLeast(1)
        val sy = surface.height.toFloat() / owner.height.coerceAtLeast(1)
        val attributes = dialog.window?.attributes ?: return
        attributes.gravity = android.view.Gravity.TOP or android.view.Gravity.START
        attributes.x = origin[0] + ((px - ox) * sx).toInt()
        attributes.y = origin[1] + ((py - oy) * sy).toInt()
        attributes.width = (info.width * sx).toInt().coerceAtLeast(32)
        attributes.height = (info.height * sy).toInt().coerceAtLeast(32)
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
                shown.renderer = (if (event.backend == "Rdp") {
                    WindowPictures(s, event.window, holder)
                } else {
                    WindowDecoder(s, event.window, holder.surface, info.width, info.height)
                }).also { it.start() }
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
        val pending = Switch(event.generation, event.backend)
        switching = pending
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
                        if (surface.visibility == View.VISIBLE) title = "${window.title}: $line"
                        if (ended) {
                            status.text = "Stream ended" + (error?.let { ": $it" } ?: "")
                            stopDecoding()
                        }
                    }
                }
            }
            pending.renderer = (if (event.backend == "Rdp") {
                WindowPictures(s, event.window, holder) { stats ->
                    if (stats.pictures > 0) onShown()
                    report("${stats.pictures} pictures over RDP, ${stats.width}x${stats.height}", stats.ended, stats.error)
                }
            } else {
                WindowDecoder(s, event.window, holder.surface, window.width, window.height) { stats ->
                    if (stats.frames > 0) onShown()
                    report("${stats.frames} frames (${stats.codec ?: event.codec})", stats.ended, stats.error)
                }
            }).also {
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
    private fun finishSwitch(pending: Switch, view: SurfaceView) {
        if (switching !== pending) return
        val s = session ?: return
        val window = watching ?: return
        switching = null
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
        val pending = switching ?: return
        switching = null
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
        val scale = minOf(
            1f,
            screen.widthPixels * 0.9f / width.coerceAtLeast(1),
            screen.heightPixels * 0.8f / height.coerceAtLeast(1),
        )
        dialog.window?.setLayout(
            (width * scale).toInt().coerceAtLeast(64),
            (height * scale).toInt().coerceAtLeast(64),
        )
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
        val reader = listener
        stopListening(wait = false)
        terminalView.detach()
        decoder?.stop()
        audio?.stop()
        stopMicrophone()
        if (keepSession && isChangingConfigurations) {
            // Recreated, not finished: the next instance takes the session and the shell over.
            // The stream does not survive its surface, so the host is told to stop it.
            val s = session
            val w = watching
            if (decoder != null && s != null && w != null) worker.execute { s.stopWindow(w.id) }
            // Nor do the windows watched beside it.
            if (s != null) for (id in extraIds) worker.execute { s.stopWindow(id) }
            reader?.join(1000)
        } else {
            terminalSession?.close()
            session?.close()
            client?.close()
        }
        worker.shutdown()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "windowcast"
        private const val WANTED_REFRESH_MS = 2_000L
        /** A game asked to start just now may take a while to open its window. */
        private const val WANTED_WITHIN_MS = 90_000L
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
        /** How long a browser sign-in may take before it is given up. */
        private const val SIGN_IN_TIMEOUT_MS = 2 * 60 * 1000
        private val REFRESH_DELAYS_MS = longArrayOf(1000, 2500, 5000, 10000)
    }
}
