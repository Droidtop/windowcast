package dev.windowcast.viewer

import android.app.Activity
import android.content.Context
import android.os.Bundle
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
import dev.windowcast.Microphone
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
        list = ListView(this).apply {
            setOnItemClickListener { _, _, position, _ -> watch(windows[position]) }
        }
        form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(address, MATCH_PARENT, WRAP_CONTENT)
            addView(pin, MATCH_PARENT, WRAP_CONTENT)
            addView(connect, MATCH_PARENT, WRAP_CONTENT)
            addView(sendMicrophone, MATCH_PARENT, WRAP_CONTENT)
            addView(status, MATCH_PARENT, WRAP_CONTENT)
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
    }
}
