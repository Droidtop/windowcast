package dev.windowcast.host

/** windowcast's Android host's C interface (agent-android/include/windowcast_host.h), one to one. */
internal object HostNative {
    init {
        System.loadLibrary("windowcast_host_android")
        System.loadLibrary("windowcast_host_jni")
    }

    @JvmStatic external fun start(dataDir: String, listen: String, pairing: Boolean): Long
    @JvmStatic external fun lastError(): String
    @JvmStatic external fun id(host: Long): String
    @JvmStatic external fun free(host: Long)
    @JvmStatic external fun setWindows(host: Long, json: String): Long
    @JvmStatic external fun setEncoders(host: Long, codecs: IntArray)
    @JvmStatic external fun frame(host: Long, window: Long, data: ByteArray, length: Int, durationUs: Long, width: Int, height: Int): Long
    @JvmStatic external fun sound(host: Long, window: Long, samples: ShortArray, count: Int): Long
    @JvmStatic external fun soundEnded(host: Long, window: Long)
    @JvmStatic external fun ended(host: Long, window: Long)
    @JvmStatic external fun nextCommand(host: Long, timeoutMs: Int): String?
}
