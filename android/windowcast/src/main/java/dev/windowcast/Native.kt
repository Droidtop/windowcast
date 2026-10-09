package dev.windowcast

import java.nio.ByteBuffer

/** The windowcast C interface (client-core/include/windowcast.h), one to one. */
internal object Native {
    init {
        System.loadLibrary("windowcast_client")
        System.loadLibrary("windowcast_jni")
    }

    const val TIMEOUT = 0L
    const val ENDED = -1L
    const val BUFFER_TOO_SMALL = -2L
    const val ERROR = -3L

    @JvmStatic external fun clientNew(dataDir: String): Long
    @JvmStatic external fun clientFree(client: Long)
    @JvmStatic external fun clientPeerId(client: Long): String
    @JvmStatic external fun connect(client: Long, address: String, pin: String?): Long
    @JvmStatic external fun lastError(): String
    @JvmStatic external fun sessionFree(session: Long)
    @JvmStatic external fun sessionHost(session: Long): String
    @JvmStatic external fun sessionPaired(session: Long): Boolean
    @JvmStatic external fun requestWindows(session: Long): Long
    @JvmStatic external fun startWindow(session: Long, window: Long, codecs: IntArray): Long
    @JvmStatic external fun stopWindow(session: Long, window: Long): Long
    @JvmStatic external fun sendInput(session: Long, json: String): Long
    @JvmStatic external fun setClipboard(session: Long, text: String): Long
    @JvmStatic external fun nextEvent(session: Long, timeoutMs: Int): String?
    @JvmStatic external fun startMicrophone(session: Long): Long
    /** Sends the first [count] samples of [samples]: interleaved stereo, 16-bit, 48 kHz. */
    @JvmStatic external fun sendMicrophone(session: Long, samples: ShortArray, count: Int): Long
    @JvmStatic external fun stopMicrophone(session: Long): Long
    /** The next Opus packet of a window's sound into [buffer]; info[0] gets its RTP timestamp. */
    @JvmStatic external fun nextAudio(
        session: Long,
        window: Long,
        timeoutMs: Int,
        buffer: ByteBuffer,
        info: IntArray,
    ): Long
    @JvmStatic external fun nextFrame(
        session: Long,
        window: Long,
        timeoutMs: Int,
        buffer: ByteBuffer,
        info: IntArray,
    ): Long
}
