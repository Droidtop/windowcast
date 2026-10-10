package dev.windowcast

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.util.Log

/**
 * Sends this device's microphone to the host on its own thread: AudioRecord
 * at 48 kHz stereo, handed to the session as it comes, which encodes it to
 * Opus with libopus (the same encoder on every client, and on every Android
 * version; MediaCodec has no Opus encoder before Android 10, nor on some
 * builds after). The caller holds RECORD_AUDIO.
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
        try {
            if (session.withHandle(Native.ERROR) { Native.startMicrophone(it) } != 0L) error("the host session refused the microphone")
            val minimum = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
            record = AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                RATE,
                AudioFormat.CHANNEL_IN_STEREO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minimum, FRAME_SAMPLES * 2 * 4),
            )
            if (record.state != AudioRecord.STATE_INITIALIZED) error("the microphone could not be opened")
            record.startRecording()
            val pcm = ShortArray(FRAME_SAMPLES)
            var sent = 0L
            while (running) {
                val n = record.read(pcm, 0, pcm.size)
                if (n < 0) error("the microphone stopped ($n)")
                if (n > 0 && session.withHandle(Native.ERROR) { Native.sendMicrophone(it, pcm, n) } != 0L) error("the session stopped taking the microphone")
                sent += n
                if (sent >= RATE * 2 * 5) {
                    Log.i(TAG, "microphone: 5 s sent")
                    sent = 0
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
            session.withHandle(Unit) { Native.stopMicrophone(it) }
        }
    }

    private companion object {
        const val TAG = "windowcast"
        const val RATE = 48_000

        /** 20 ms of interleaved stereo samples. */
        const val FRAME_SAMPLES = RATE / 50 * 2
    }
}
