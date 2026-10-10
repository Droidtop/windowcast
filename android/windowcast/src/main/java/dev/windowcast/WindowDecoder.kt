package dev.windowcast

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Decodes one streamed window onto a [Surface] with MediaCodec, on its own
 * thread: pulls frames from the session, hands parameter sets to the
 * decoder as codec config, and renders every frame as soon as it decodes
 * (no timestamp pacing: the host already paces, and waiting only adds
 * latency).
 */
class WindowDecoder(
    private val session: WindowcastSession,
    private val window: Long,
    private val surface: Surface,
    private val width: Int,
    private val height: Int,
    /** Called on the decoder thread with frames rendered so far. */
    private val onProgress: (Stats) -> Unit = {},
) : WindowRenderer {
    data class Stats(val frames: Long, val keyframes: Long, val codec: Codec?, val ended: Boolean, val error: String?)

    @Volatile private var running = true
    private val thread = Thread(::run, "windowcast-decoder-$window")

    override fun start() = thread.start()

    override fun stop() {
        running = false
        thread.join(2000)
    }

    private fun run() {
        var buffer = ByteBuffer.allocateDirect(2 * 1024 * 1024)
        val info = IntArray(4)
        var codec: MediaCodec? = null
        var codecType: Codec? = null
        var frames = 0L
        var keyframes = 0L
        var error: String? = null
        val output = MediaCodec.BufferInfo()
        try {
            while (running) {
                val len = session.withHandle(Native.ERROR) { Native.nextFrame(it, window, 200, buffer, info) }
                when {
                    len == Native.TIMEOUT -> continue
                    len == Native.BUFFER_TOO_SMALL -> {
                        buffer = ByteBuffer.allocateDirect(info[3] * 2)
                        continue
                    }
                    len < 0 -> break
                }
                val type = Codec.fromId(info[0]) ?: break
                val keyframe = info[1] != 0
                val data = ByteArray(len.toInt())
                buffer.position(0)
                buffer.get(data)

                if (codec == null) {
                    if (!keyframe) continue
                    codec = MediaCodec.createDecoderByType(type.mime).apply {
                        configure(MediaFormat.createVideoFormat(type.mime, width.coerceAtLeast(16), height.coerceAtLeast(16)), surface, null, 0)
                        start()
                    }
                    codecType = type
                    // H.264/H.265 parameter sets go in as codec config, so
                    // every decoder takes them whatever it does with inline ones.
                    val config = parameterSets(type, data)
                    if (config.isNotEmpty()) queue(codec, config, 0, MediaCodec.BUFFER_FLAG_CODEC_CONFIG)
                }
                val rtpMicros = (info[2].toLong() and 0xffffffffL) * 1000 / 90
                queue(codec, data, rtpMicros, if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
                frames++
                if (keyframe) keyframes++

                while (true) {
                    val index = codec.dequeueOutputBuffer(output, 0)
                    if (index >= 0) codec.releaseOutputBuffer(index, true) else break
                }
                if (frames % 30 == 0L) onProgress(Stats(frames, keyframes, codecType, false, null))
            }
        } catch (e: Exception) {
            Log.e(TAG, "decoding window $window failed", e)
            error = e.toString()
        } finally {
            codec?.let {
                runCatching { it.stop() }
                it.release()
            }
            onProgress(Stats(frames, keyframes, codecType, true, error))
        }
    }

    private fun queue(codec: MediaCodec, data: ByteArray, timeUs: Long, flags: Int) {
        val index = codec.dequeueInputBuffer(100_000)
        if (index < 0) return
        val input = codec.getInputBuffer(index) ?: return
        input.clear()
        input.put(data)
        codec.queueInputBuffer(index, 0, data.size, timeUs, flags)
    }

    companion object {
        private const val TAG = "windowcast"

        /**
         * Codecs this device decodes, most preferred first: hardware decoders
         * by compression (AV1, H.265, H.264), then software H.264, which every
         * device has.
         */
        fun decodableCodecs(): List<Codec> {
            val decoders = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { !it.isEncoder }
            fun has(codec: Codec, hardware: Boolean) = decoders.any { info ->
                info.supportedTypes.any { it.equals(codec.mime, ignoreCase = true) } &&
                    isHardware(info) == hardware
            }
            val ordered = listOf(Codec.AV1, Codec.H265, Codec.H264).filter { has(it, hardware = true) }
            return if (Codec.H264 in ordered) ordered else ordered + Codec.H264
        }

        private fun isHardware(info: android.media.MediaCodecInfo): Boolean =
            if (Build.VERSION.SDK_INT >= 29) {
                info.isHardwareAccelerated
            } else {
                val name = info.name.lowercase()
                !name.startsWith("omx.google.") && !name.startsWith("c2.android.")
            }

        /** The SPS/PPS (H.264) or VPS/SPS/PPS (H.265) NAL units of an Annex-B access unit. */
        fun parameterSets(codec: Codec, annexB: ByteArray): ByteArray {
            if (codec == Codec.AV1) return ByteArray(0)
            val out = java.io.ByteArrayOutputStream()
            val starts = mutableListOf<Int>()
            var i = 0
            while (i + 3 <= annexB.size) {
                if (annexB[i].toInt() == 0 && annexB[i + 1].toInt() == 0 && annexB[i + 2].toInt() == 1) {
                    starts += i + 3
                    i += 3
                } else {
                    i++
                }
            }
            for ((n, start) in starts.withIndex()) {
                if (start >= annexB.size) continue
                var end = if (n + 1 < starts.size) starts[n + 1] - 3 else annexB.size
                while (end > start && annexB[end - 1].toInt() == 0) end--
                val header = annexB[start].toInt() and 0xff
                val isParameterSet = when (codec) {
                    Codec.H264 -> (header and 0x1f) in 7..8
                    Codec.H265 -> ((header shr 1) and 0x3f) in 32..34
                    Codec.AV1 -> false
                }
                if (isParameterSet) {
                    out.write(byteArrayOf(0, 0, 0, 1))
                    out.write(annexB, start, end - start)
                }
            }
            return out.toByteArray()
        }
    }
}
