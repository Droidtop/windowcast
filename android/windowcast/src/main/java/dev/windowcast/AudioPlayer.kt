package dev.windowcast

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Plays one streamed window's sound on its own thread: Opus packets from
 * the session decoded by MediaCodec and written to a low-latency
 * AudioTrack as they come (the host paces them; a small device buffer
 * absorbs the rest). A window without sound simply never gets a packet.
 */
class AudioPlayer(
    private val session: WindowcastSession,
    private val window: Long,
    /** Called on the audio thread every 50 packets (one second) played. */
    private val onProgress: (packets: Long) -> Unit = {},
) {
    @Volatile private var running = true
    @Volatile var muted = false
    private val thread = Thread(::run, "windowcast-audio-$window")

    fun start() = thread.start()

    fun stop() {
        running = false
        thread.join(2000)
    }

    private fun run() {
        val buffer = ByteBuffer.allocateDirect(4096)
        val info = IntArray(1)
        var codec: MediaCodec? = null
        var track: AudioTrack? = null
        val output = MediaCodec.BufferInfo()
        var packets = 0L
        try {
            while (running) {
                val len = Native.nextAudio(session.handle, window, 100, buffer, info)
                if (len == Native.TIMEOUT || len == Native.BUFFER_TOO_SMALL) continue
                if (len < 0) break
                val packet = ByteArray(len.toInt())
                buffer.position(0)
                buffer.get(packet)
                if (codec == null) {
                    codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).apply {
                        configure(opusFormat(), null, null, 0)
                        start()
                    }
                    track = AudioTrack.Builder()
                        .setAudioAttributes(
                            AudioAttributes.Builder()
                                .setUsage(AudioAttributes.USAGE_MEDIA)
                                .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                                .build(),
                        )
                        .setAudioFormat(
                            AudioFormat.Builder()
                                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                                .setSampleRate(RATE)
                                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                                .build(),
                        )
                        .setBufferSizeInBytes(
                            AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT) * 2,
                        )
                        .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                        .setTransferMode(AudioTrack.MODE_STREAM)
                        .build()
                        .also { it.play() }
                }
                val rtpMicros = (info[0].toLong() and 0xffffffffL) * 1000 / 48
                val index = codec.dequeueInputBuffer(20_000)
                if (index >= 0) {
                    codec.getInputBuffer(index)?.let { input ->
                        input.clear()
                        input.put(packet)
                        codec.queueInputBuffer(index, 0, packet.size, rtpMicros, 0)
                    }
                }
                packets++
                if (packets % 50 == 0L) {
                    Log.i(TAG, "window $window: $packets Opus packets played")
                    onProgress(packets)
                }
                while (true) {
                    val out = codec.dequeueOutputBuffer(output, 0)
                    if (out < 0) break
                    val pcm = codec.getOutputBuffer(out)
                    if (pcm != null && output.size > 0 && !muted) {
                        pcm.position(output.offset)
                        pcm.limit(output.offset + output.size)
                        track?.write(pcm, output.size, AudioTrack.WRITE_NON_BLOCKING)
                    }
                    codec.releaseOutputBuffer(out, false)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "playing window $window's sound failed", e)
        } finally {
            codec?.let {
                runCatching { it.stop() }
                it.release()
            }
            track?.let {
                runCatching { it.stop() }
                it.release()
            }
        }
    }

    companion object {
        private const val TAG = "windowcast"
        private const val RATE = 48_000

        /**
         * MediaCodec's Opus decoder wants the stream's identification
         * header (RFC 7845 "OpusHead") as csd-0, and the pre-skip and seek
         * pre-roll in nanoseconds as csd-1 and csd-2.
         */
        private fun opusFormat(): MediaFormat {
            val preSkip = 312
            val head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN).apply {
                put("OpusHead".toByteArray(Charsets.US_ASCII))
                put(1) // version
                put(2) // channels
                putShort(preSkip.toShort())
                putInt(RATE)
                putShort(0) // output gain
                put(0) // mapping family: mono or stereo
                flip()
            }
            fun nanos(value: Long) =
                ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(value).apply { flip() }
            return MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, RATE, 2).apply {
                setByteBuffer("csd-0", head)
                setByteBuffer("csd-1", nanos(preSkip * 1_000_000_000L / RATE))
                setByteBuffer("csd-2", nanos(80_000_000L))
            }
        }
    }
}
