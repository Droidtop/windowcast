package dev.windowcast

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.MediaRecorder
import android.util.Log

/**
 * Sends this device's microphone to the host on its own thread: AudioRecord
 * at 48 kHz stereo, encoded to Opus by MediaCodec (Android 10 and later
 * ship an Opus encoder), one packet at a time over the session. The caller
 * holds RECORD_AUDIO.
 */
class Microphone(private val session: WindowcastSession) {
    @Volatile private var running = true
    @Volatile var error: String? = null
        private set
    private val thread = Thread(::run, "windowcast-microphone")

    fun start() = thread.start()

    fun stop() {
        running = false
        thread.join(2000)
    }

    @SuppressLint("MissingPermission")
    private fun run() {
        var record: AudioRecord? = null
        var codec: MediaCodec? = null
        try {
            if (Native.startMicrophone(session.handle) != 0L) error("the host session refused the microphone")
            val minimum = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
            record = AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                RATE,
                AudioFormat.CHANNEL_IN_STEREO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minimum, FRAME_BYTES * 4),
            )
            codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).apply {
                configure(
                    MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, RATE, 2).apply {
                        setInteger(MediaFormat.KEY_BIT_RATE, 64_000)
                        setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, FRAME_BYTES)
                    },
                    null,
                    null,
                    MediaCodec.CONFIGURE_FLAG_ENCODE,
                )
                start()
            }
            record.startRecording()
            val pcm = ByteArray(FRAME_BYTES)
            val output = MediaCodec.BufferInfo()
            val packet = java.nio.ByteBuffer.allocateDirect(4096)
            var time = 0L
            while (running) {
                var read = 0
                while (read < pcm.size && running) {
                    val n = record.read(pcm, read, pcm.size - read)
                    if (n < 0) error("the microphone stopped ($n)")
                    read += n
                }
                val index = codec.dequeueInputBuffer(20_000)
                if (index >= 0) {
                    codec.getInputBuffer(index)?.let { input ->
                        input.clear()
                        input.put(pcm, 0, read)
                        codec.queueInputBuffer(index, 0, read, time, 0)
                        time += 20_000
                    }
                }
                while (true) {
                    val out = codec.dequeueOutputBuffer(output, 0)
                    if (out < 0) break
                    val encoded = codec.getOutputBuffer(out)
                    // The first output is the stream header (codec config), not sound.
                    if (encoded != null && output.size > 0 && output.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                        encoded.position(output.offset)
                        encoded.limit(output.offset + output.size)
                        packet.clear()
                        packet.put(encoded)
                        Native.sendMicrophone(session.handle, packet, output.size)
                    }
                    codec.releaseOutputBuffer(out, false)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "sending the microphone failed", e)
            error = e.message ?: e.toString()
        } finally {
            record?.let {
                runCatching { it.stop() }
                it.release()
            }
            codec?.let {
                runCatching { it.stop() }
                it.release()
            }
            Native.stopMicrophone(session.handle)
        }
    }

    companion object {
        private const val TAG = "windowcast"
        private const val RATE = 48_000

        /** 20 ms of 16-bit stereo. */
        private const val FRAME_BYTES = RATE / 50 * 2 * 2

        /** Whether this device can encode Opus (Android 10 and later). */
        fun available(): Boolean =
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { info ->
                info.isEncoder && info.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_AUDIO_OPUS, ignoreCase = true) }
            }
    }
}
