package dev.windowcast.host

import android.app.ActivityOptions
import android.content.Context
import android.content.Intent
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import java.io.Closeable
import java.io.File
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

/**
 * windowcast's Android host (Droidtop/tracker#615): each app on a virtual display of its own,
 * drawn straight into a MediaCodec encoder's input surface, served by windowcast's host
 * (agent-android) as one window per app.
 *
 * It must run as the shell user, through Shizuku or Sui (`UserService`), since only shell may
 * make a display other apps launch onto and inject input into it. [context] is the service's.
 * The host listens on [listen], loopback by default: what the client inside the device's own
 * Linux container needs. Pairing is the windowcast identity in [dataDir].
 *
 * Built so far: the displays, the encoders, the frames and the host's commands. Input injection,
 * per-app sound and raw pictures for loopback are the next steps on the issue.
 */
class AndroidHost(
    private val context: Context,
    dataDir: File,
    listen: String = "127.0.0.1:$DEFAULT_PORT",
    pairing: Boolean = false,
) : Closeable {
    private val handle: Long = HostNative.start(dataDir.path, listen, pairing)
        .also { if (it == 0L) throw IOException(HostNative.lastError()) }

    private val displays = context.getSystemService(DisplayManager::class.java)
    private val nextWindow = AtomicLong(1)
    private val apps = ConcurrentHashMap<Long, Shown>()

    @Volatile private var closed = false

    /** One app's display, and its encoder while someone watches it. */
    private class Shown(val app: HostApp, val display: VirtualDisplay) {
        @Volatile var encoder: Encoder? = null
    }

    private val commands = Thread({ takeCommands() }, "windowcast-host-commands")

    init {
        HostNative.setEncoders(handle, encoderIds())
        commands.start()
    }

    /**
     * Starts [packageName] on a display of its own, [width] by [height] at [dpi], and lists it
     * as a window. The window's id, or an exception saying why (no such app, no display).
     */
    @Throws(IOException::class)
    fun openApp(packageName: String, width: Int, height: Int, dpi: Int): Long {
        val launch = context.packageManager.getLaunchIntentForPackage(packageName)
            ?: throw IOException("$packageName has nothing to open")
        val title = runCatching {
            context.packageManager.getApplicationLabel(context.packageManager.getApplicationInfo(packageName, 0)).toString()
        }.getOrDefault(packageName)
        val window = nextWindow.getAndIncrement()
        // No surface until someone watches: the app runs, and nothing is drawn for nobody.
        val display = displays.createVirtualDisplay("windowcast-$window", width, height, dpi, null, DISPLAY_FLAGS)
            ?: throw IOException("no display could be made for $packageName")
        apps[window] = Shown(HostApp(window, packageName, title, width, height), display)
        publish()
        val options = ActivityOptions.makeBasic().setLaunchDisplayId(display.display.displayId)
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_MULTIPLE_TASK)
        context.startActivity(launch, options.toBundle())
        return window
    }

    /** Ends [window]: its encoder, its display, and its app's place in the list. */
    fun closeApp(window: Long) {
        val shown = apps.remove(window) ?: return
        shown.encoder?.stop()
        shown.display.release()
        HostNative.ended(handle, window)
        publish()
    }

    override fun close() {
        if (closed) return
        closed = true
        for (window in apps.keys.toList()) closeApp(window)
        commands.join(COMMAND_WAIT_MS * 2L)
        HostNative.free(handle)
    }

    private fun publish() {
        HostNative.setWindows(handle, windowsJson(apps.values.map { it.app }))
    }

    private fun takeCommands() {
        while (!closed) {
            val json = HostNative.nextCommand(handle, COMMAND_WAIT_MS) ?: continue
            val command = HostCommand.parse(json) ?: continue
            val shown = apps[command.window] ?: continue
            try {
                when (command) {
                    is HostCommand.Open -> {
                        shown.encoder?.stop()
                        val encoder = Encoder(handle, shown, command.codec)
                        shown.encoder = encoder
                        encoder.start()
                    }
                    is HostCommand.Close -> {
                        shown.encoder?.stop()
                        shown.encoder = null
                    }
                    is HostCommand.Keyframe -> shown.encoder?.keyframe()
                    is HostCommand.Bitrate -> shown.encoder?.bitrate(command.bitrate)
                }
            } catch (e: Exception) {
                // MediaCodec reports a failed configure or start as IllegalStateException or
                // MediaCodec.CodecException; the window's stream ends and the host says so.
                Log.w(TAG, "window ${command.window}: ${command::class.simpleName} failed", e)
                shown.encoder?.stop()
                shown.encoder = null
                HostNative.ended(handle, command.window)
            }
        }
    }

    /** One window's hardware encoder, fed by its display through the encoder's input surface. */
    private class Encoder(private val host: Long, private val shown: Shown, codecName: String) {
        private val mime = HostCommand.mime(codecName) ?: throw IllegalArgumentException("unknown codec $codecName")
        private val codec = MediaCodec.createEncoderByType(mime)
        @Volatile private var running = true
        private val drain = Thread({ drain() }, "windowcast-host-encode-${shown.app.window}")

        fun start() {
            val format = MediaFormat.createVideoFormat(mime, shown.app.width, shown.app.height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, START_BITRATE)
                setInteger(MediaFormat.KEY_FRAME_RATE, FRAME_RATE)
                // Keyframes when the host asks; one every ten seconds otherwise.
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 10)
                // A still app sends its last picture again, so a viewer that joins sees it.
                setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 1_000_000L / FRAME_RATE * 6)
            }
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            shown.display.surface = codec.createInputSurface()
            codec.start()
            drain.start()
        }

        fun keyframe() {
            codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
        }

        fun bitrate(bitsPerSecond: Int) {
            codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bitsPerSecond) })
        }

        fun stop() {
            // The display stops drawing into the encoder before the encoder goes.
            shown.display.surface = null
            running = false
            if (Thread.currentThread() != drain) drain.join(1000)
            runCatching { codec.stop() }
            codec.release()
        }

        /** MediaCodec's output to the host: parameter sets kept and put before each keyframe. */
        private fun drain() {
            val info = MediaCodec.BufferInfo()
            var config = ByteArray(0)
            var sizeSent = false
            var lastUs = -1L
            var buffer = ByteArray(256 * 1024)
            while (running) {
                val index = try {
                    codec.dequeueOutputBuffer(info, DRAIN_WAIT_US)
                } catch (e: IllegalStateException) {
                    break
                }
                if (index < 0) continue
                val out = codec.getOutputBuffer(index)
                if (out == null || info.size == 0) {
                    codec.releaseOutputBuffer(index, false)
                    continue
                }
                out.position(info.offset)
                out.limit(info.offset + info.size)
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                    config = ByteArray(info.size).also { out.get(it) }
                    codec.releaseOutputBuffer(index, false)
                    continue
                }
                val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                val prefix = if (key) config else ByteArray(0)
                val length = prefix.size + info.size
                if (buffer.size < length) buffer = ByteArray(length)
                System.arraycopy(prefix, 0, buffer, 0, prefix.size)
                out.get(buffer, prefix.size, info.size)
                codec.releaseOutputBuffer(index, false)
                val durationUs = if (lastUs < 0) 1_000_000L / FRAME_RATE else (info.presentationTimeUs - lastUs).coerceAtLeast(0)
                lastUs = info.presentationTimeUs
                val (w, h) = if (sizeSent) 0 to 0 else shown.app.width to shown.app.height
                sizeSent = true
                if (HostNative.frame(host, shown.app.window, buffer, length, durationUs, w, h) == 0L) break
            }
        }
    }

    private companion object {
        const val TAG = "windowcast.AndroidHost"

        /** windowcast's own port is 47100; the Android host sits beside it so both can run. */
        const val DEFAULT_PORT = 47110
        const val COMMAND_WAIT_MS = 250
        const val DRAIN_WAIT_US = 100_000L
        const val FRAME_RATE = 60
        const val START_BITRATE = 8_000_000

        /**
         * Public (other apps may launch onto it), own content only (it never mirrors another
         * display), trusted (`VIRTUAL_DISPLAY_FLAG_TRUSTED`, hidden, 1 shl 10: shell holds
         * ADD_TRUSTED_DISPLAY, and without it Android 10+ blocks input and the IME there), and
         * its tasks go with it (`DESTROY_CONTENT_ON_REMOVAL`, hidden, 1 shl 8). scrcpy's
         * `--new-display` makes its displays with the same flags.
         */
        const val DISPLAY_FLAGS = DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC or
            DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY or (1 shl 10) or (1 shl 8)

        /** The codecs this device has hardware or software encoders for, as the host numbers them. */
        fun encoderIds(): IntArray {
            val have = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { it.isEncoder }
                .flatMap { it.supportedTypes.toList() }
                .toSet()
            return listOfNotNull(
                1.takeIf { "video/avc" in have },
                2.takeIf { "video/hevc" in have },
                3.takeIf { "video/av01" in have },
            ).toIntArray()
        }
    }
}
