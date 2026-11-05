package dev.windowcast.host

import android.media.AudioFormat
import android.media.AudioRecord
import android.util.Log

/**
 * One app's sound, captured by its UID (Droidtop/tracker#615): an `AudioMix` whose rule matches
 * the app's UID, looped back into an `AudioRecord`, read as stereo 16-bit at 48 kHz and handed to
 * the host, which encodes it with Opus. The mix, its rule and `AudioRecord.Builder.setAudioMix`
 * are system APIs, reached by reflection; the shell user may route audio this way (scrcpy's
 * playback capture uses the same route). An app that opts out of capture
 * (`allowAudioPlaybackCapture`) is silent, as Android decides.
 */
internal class AppSound(private val host: Long, private val window: Long, uid: Int) {
    private val record: AudioRecord = build(uid)
    @Volatile private var running = true
    private val reader = Thread({ read() }, "windowcast-host-sound-$window")

    fun start() {
        record.startRecording()
        reader.start()
    }

    fun stop() {
        running = false
        if (Thread.currentThread() != reader) reader.join(1000)
        runCatching { record.stop() }
        record.release()
    }

    private fun read() {
        // 20 ms of stereo at a time: the host's Opus packet length.
        val buffer = ShortArray(SAMPLE_RATE / 50 * 2)
        while (running) {
            val n = record.read(buffer, 0, buffer.size)
            if (n < 0) {
                Log.w(TAG, "window $window: the app's sound stopped ($n)")
                HostNative.soundEnded(host, window)
                break
            }
            if (n > 0 && HostNative.sound(host, window, buffer, n) == 0L) break
        }
    }

    private companion object {
        const val TAG = "windowcast.AndroidHost"
        const val SAMPLE_RATE = 48_000

        /** `AudioMixingRule.RULE_MATCH_UID`, `MIX_ROLE_PLAYERS`, `AudioMix.ROUTE_FLAG_LOOP_BACK`. */
        const val RULE_MATCH_UID = 4
        const val MIX_ROLE_PLAYERS = 0
        const val ROUTE_FLAG_LOOP_BACK = 2

        fun build(uid: Int): AudioRecord {
            val format = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(SAMPLE_RATE)
                .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                .build()
            val ruleBuilderClass = Class.forName("android.media.audiopolicy.AudioMixingRule\$Builder")
            val ruleBuilder = ruleBuilderClass.getConstructor().newInstance()
            ruleBuilderClass.getMethod("setTargetMixRole", Int::class.javaPrimitiveType).invoke(ruleBuilder, MIX_ROLE_PLAYERS)
            ruleBuilderClass.getMethod("addMixRule", Int::class.javaPrimitiveType, Any::class.java)
                .invoke(ruleBuilder, RULE_MATCH_UID, uid)
            val rule = ruleBuilderClass.getMethod("build").invoke(ruleBuilder)
            val ruleClass = Class.forName("android.media.audiopolicy.AudioMixingRule")
            val mixBuilderClass = Class.forName("android.media.audiopolicy.AudioMix\$Builder")
            val mixBuilder = mixBuilderClass.getConstructor(ruleClass).newInstance(rule)
            mixBuilderClass.getMethod("setFormat", AudioFormat::class.java).invoke(mixBuilder, format)
            mixBuilderClass.getMethod("setRouteFlags", Int::class.javaPrimitiveType).invoke(mixBuilder, ROUTE_FLAG_LOOP_BACK)
            val mix = mixBuilderClass.getMethod("build").invoke(mixBuilder)
            val recordBuilder = AudioRecord.Builder().setAudioFormat(format)
            AudioRecord.Builder::class.java.getMethod("setAudioMix", Class.forName("android.media.audiopolicy.AudioMix"))
                .invoke(recordBuilder, mix)
            return recordBuilder.build()
        }
    }
}
