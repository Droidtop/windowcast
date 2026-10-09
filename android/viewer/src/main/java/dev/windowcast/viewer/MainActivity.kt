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
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ListView
import android.widget.TextView
import dev.windowcast.Codec
import dev.windowcast.Event
import dev.windowcast.WindowDecoder
import dev.windowcast.WindowInfo
import dev.windowcast.WindowcastClient
import dev.windowcast.WindowcastSession
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

    private lateinit var address: EditText
    private lateinit var pin: EditText
    private lateinit var status: TextView
    private lateinit var list: ListView
    private lateinit var surface: SurfaceView
    private lateinit var form: LinearLayout

    private var client: WindowcastClient? = null
    private var session: WindowcastSession? = null
    private var decoder: WindowDecoder? = null
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
        list = ListView(this).apply {
            setOnItemClickListener { _, _, position, _ -> watch(windows[position]) }
        }
        form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 32, 32, 32)
            addView(address, MATCH_PARENT, WRAP_CONTENT)
            addView(pin, MATCH_PARENT, WRAP_CONTENT)
            addView(connect, MATCH_PARENT, WRAP_CONTENT)
            addView(status, MATCH_PARENT, WRAP_CONTENT)
            addView(list, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        surface = SurfaceView(this).apply { visibility = View.GONE }
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
                session?.close()
                val s = client!!.connect(address, pin)
                session = s
                main.post {
                    status.text = (if (s.paired) "Paired with " else "Connected to ") + s.hostId.take(16) + "…"
                }
                listen(s)
                s.requestWindows()
            } catch (e: Exception) {
                main.post { status.text = "Could not connect: ${e.message}" }
            }
        }
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
                    is Event.Closed -> {
                        main.post { status.text = "Disconnected"; stopDecoding() }
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
            decoder = WindowDecoder(s, event.window, surface.holder.surface, window.width, window.height) { stats ->
                main.post {
                    title = "${window.title}: ${stats.frames} frames (${stats.codec ?: event.codec})"
                    if (stats.ended) {
                        status.text = "Stream ended after ${stats.frames} frames" +
                            (stats.error?.let { ": $it" } ?: "")
                        showForm()
                    }
                }
            }.also { it.start() }
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

    private fun stopDecoding() {
        decoder?.let { d -> worker.execute { d.stop() } }
        decoder = null
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
        listening = false
        decoder?.stop()
        session?.close()
        client?.close()
        worker.shutdown()
        super.onDestroy()
    }
}
