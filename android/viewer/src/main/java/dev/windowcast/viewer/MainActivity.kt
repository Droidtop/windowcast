package dev.windowcast.viewer

import android.app.Activity
import android.app.AlertDialog
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
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ListView
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
    private lateinit var list: ListView
    private lateinit var surface: SurfaceView
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
    private lateinit var form: LinearLayout
    private lateinit var sendMicrophone: CheckBox
    @Volatile private var microphone: Microphone? = null

    private var client: WindowcastClient? = null
    private var session: WindowcastSession? = null
    private var decoder: WindowRenderer? = null
    private var audio: AudioPlayer? = null
    @Volatile private var soundPackets = 0L
    private var windows: List<WindowInfo> = emptyList()
    private var watching: WindowInfo? = null
    @Volatile private var listening = false

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
        list = ListView(this).apply {
            setOnItemClickListener { _, _, position, _ -> watch(windows[position]) }
        }
        form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(address, MATCH_PARENT, WRAP_CONTENT)
            addView(pin, MATCH_PARENT, WRAP_CONTENT)
            addView(connect, MATCH_PARENT, WRAP_CONTENT)
            addView(signInButton, MATCH_PARENT, WRAP_CONTENT)
            addView(sendMicrophone, MATCH_PARENT, WRAP_CONTENT)
            addView(status, MATCH_PARENT, WRAP_CONTENT)
            addView(shell, MATCH_PARENT, WRAP_CONTENT)
            addView(launchLine, MATCH_PARENT, WRAP_CONTENT)
            addView(launch, MATCH_PARENT, WRAP_CONTENT)
            addView(sshUser, MATCH_PARENT, WRAP_CONTENT)
            addView(sshHost, MATCH_PARENT, WRAP_CONTENT)
            addView(sshPort, MATCH_PARENT, WRAP_CONTENT)
            addView(sshPassword, MATCH_PARENT, WRAP_CONTENT)
            addView(sshButton, MATCH_PARENT, WRAP_CONTENT)
            addView(list, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        surface = SurfaceView(this).apply {
            visibility = View.GONE
            setOnTouchListener { view, event -> touch(view, event) }
        }
        clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.addPrimaryClipChangedListener {
            val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString()
            val s = session
            if (text != null && text != fromHost && s != null) inputWorker.execute { s.setClipboard(text) }
        }
        setContentView(FrameLayout(this).apply {
            addView(form, MATCH_PARENT, MATCH_PARENT)
            addView(surface, MATCH_PARENT, MATCH_PARENT)
            addView(terminalView, MATCH_PARENT, MATCH_PARENT)
        })

        client = WindowcastClient(File(filesDir, "windowcast"))
        status.text = "This device: ${client?.peerId?.take(16)}…"
    }

    private fun connect(address: String, pin: String?) {
        status.text = "Connecting to $address…"
        worker.execute {
            try {
                stopMicrophone()
                session?.close()
                val s = client!!.connect(address, pin)
                // Windows the rules send to RDP (text) come as pictures.
                s.acceptPictures(true)
                session = s
                main.post {
                    status.text = (if (s.paired) "Paired with " else "Connected to ") + s.hostId.take(16) + "…"
                    if (sendMicrophone.isChecked) startMicrophone()
                }
                listen(s)
                s.requestWindows()
            } catch (e: Exception) {
                main.post { status.text = "Could not connect: ${e.message}" }
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
                    backToFront()
                    connectAccount(address, SignIn.Oidc(provider, token), accept)
                }
            } catch (e: Exception) {
                main.post { status.text = "Could not sign in with ${provider.name}: ${e.message}" }
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
        val c = client ?: return
        status.text = "Signing in to $address…"
        // The same session start as connect() (Droidtop/tracker#447 changes how the old
        // session is closed there; this follows it when that lands).
        worker.execute {
            try {
                stopMicrophone()
                session?.close()
                val s = c.connectAccount(address, signIn, accept)
                s.acceptPictures(true)
                session = s
                main.post {
                    status.text = "Signed in to " + s.hostId.take(16) + "…"
                    if (sendMicrophone.isChecked) startMicrophone()
                }
                listen(s)
                s.requestWindows()
            } catch (e: Exception) {
                main.post { status.text = "Could not sign in: ${e.message}" }
            }
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

    /** Reads session events on their own thread. */
    private fun listen(s: WindowcastSession) {
        listening = true
        Thread({
            while (listening) {
                when (val event = s.nextEvent(500) ?: continue) {
                    is Event.Windows -> main.post { showWindows(event.windows) }
                    is Event.StreamStarted -> main.post { startDecoding(event) }
                    is Event.StreamRefused -> main.post { status.text = "Refused: ${event.reason}"; showForm() }
                    is Event.StreamStopped -> main.post { stopDecoding() }
                    is Event.Clipboard -> main.post {
                        fromHost = event.text
                        clipboard.setPrimaryClip(ClipData.newPlainText("windowcast", event.text))
                    }
                    is Event.Closed -> {
                        main.post { status.text = "Disconnected"; stopMicrophone(); stopDecoding() }
                        break
                    }
                    else -> {}
                }
            }
        }, "windowcast-events").start()
    }

    private fun showWindows(list: List<WindowInfo>) {
        windows = list
        this.list.adapter = ArrayAdapter(
            this,
            android.R.layout.simple_list_item_1,
            list.map { "${it.title}  (${it.appId}, ${it.width}x${it.height})" },
        )
        status.text = "${list.size} windows. Tap one to watch it."
    }

    private fun watch(window: WindowInfo) {
        val s = session ?: return
        watching = window
        status.text = "Starting ${window.title}…"
        val codecs = WindowDecoder.decodableCodecs()
        worker.execute { s.startWindow(window.id, codecs) }
    }

    private fun startDecoding(event: Event.StreamStarted) {
        val window = watching ?: return
        val s = session ?: return
        form.visibility = View.GONE
        surface.visibility = View.VISIBLE
        val begin = {
            decoder = if (event.backend == "Rdp") {
                WindowPictures(s, event.window, surface.holder) { stats ->
                    main.post {
                        title = "${window.title}: ${stats.pictures} pictures over RDP, ${stats.width}x${stats.height}"
                        if (stats.ended) {
                            status.text = "Stream ended after ${stats.pictures} pictures" +
                                (stats.error?.let { ": $it" } ?: "")
                            showForm()
                        }
                    }
                }
            } else {
                WindowDecoder(s, event.window, surface.holder.surface, window.width, window.height) { stats ->
                    main.post {
                        title = "${window.title}: ${stats.frames} frames (${stats.codec ?: event.codec}), sound $soundPackets packets"
                        if (stats.ended) {
                            status.text = "Stream ended after ${stats.frames} frames" +
                                (stats.error?.let { ": $it" } ?: "")
                            showForm()
                        }
                    }
                }
            }.also { it.start() }
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

    /** Starts an application on the connected host; its windows arrive in the list. */
    private fun launchOnHost(line: String) {
        val s = session ?: run { status.text = "Connect to a host first"; return }
        val argv = line.trim().split(" ").filter { it.isNotEmpty() }
        if (argv.isEmpty()) {
            status.text = "Type a program to start"
            return
        }
        worker.execute {
            try {
                val pid = s.launch(argv)
                s.requestWindows()
                main.post { status.text = "Started ${argv[0]}" + if (pid > 0) " (process $pid)" else "" }
            } catch (e: Exception) {
                main.post { status.text = "Could not start it: ${e.message}" }
            }
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
        val t = terminalSession
        terminalSession = null
        if (t != null) worker.execute { t.close() }
    }

    private fun streaming(): Boolean = surface.visibility == View.VISIBLE && watching != null

    private fun send(input: Input) {
        val s = session ?: return
        inputWorker.execute { s.send(input) }
    }

    /** Every finger, as touches on the streamed window. */
    private fun touch(view: View, event: MotionEvent): Boolean {
        val window = watching?.id ?: return false
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
        if (!streaming() || event.keyCode == KeyEvent.KEYCODE_BACK) return super.dispatchKeyEvent(event)
        if (gamepads.onKey(event)) return true
        val down = event.action == KeyEvent.ACTION_DOWN
        val evdev = Keys.evdev(event.keyCode)
        when {
            evdev != null -> if (event.repeatCount == 0) send(Input.key(evdev, down))
            down && event.unicodeChar != 0 -> send(Input.text(String(Character.toChars(event.unicodeChar))))
            else -> return super.dispatchKeyEvent(event)
        }
        return true
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streaming() && gamepads.onMotion(event)) return true
        return super.dispatchGenericMotionEvent(event)
    }

    private fun stopDecoding() {
        gamepads.releaseAll()
        decoder?.let { d -> worker.execute { d.stop() } }
        decoder = null
        audio?.let { a -> worker.execute { a.stop() } }
        audio = null
        showForm()
    }

    private fun showForm() {
        surface.visibility = View.GONE
        form.visibility = View.VISIBLE
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
        } else {
            super.onBackPressed()
        }
    }

    override fun onDestroy() {
        inputWorker.shutdown()
        listening = false
        terminalView.detach()
        terminalSession?.close()
        decoder?.stop()
        audio?.stop()
        stopMicrophone()
        session?.close()
        client?.close()
        worker.shutdown()
        super.onDestroy()
    }

    companion object {
        private const val RECORD_REQUEST = 1
        /** How long a browser sign-in may take before it is given up. */
        private const val SIGN_IN_TIMEOUT_MS = 5 * 60 * 1000
    }
}
