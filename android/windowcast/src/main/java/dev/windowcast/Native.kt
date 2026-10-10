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
    @JvmStatic external fun acceptPictures(session: Long, on: Boolean): Long
    /** The next RGBA picture of an RDP window into [buffer]; size gets its width and height. */
    @JvmStatic external fun nextPicture(
        session: Long,
        window: Long,
        timeoutMs: Int,
        buffer: ByteBuffer,
        size: IntArray,
    ): Long
    @JvmStatic external fun nextFrame(
        session: Long,
        window: Long,
        timeoutMs: Int,
        buffer: ByteBuffer,
        info: IntArray,
    ): Long

    // The command stream: terminals and launches (windowcast.h).
    @JvmStatic external fun openTerminal(session: Long, cols: Int, rows: Int): Long
    /** authKind: 0 password, 1 key (PEM); policy: [HostKeyPolicy.id]. 0 on failure, see lastError and lastFingerprint. */
    @JvmStatic external fun sshTerminal(
        client: Long,
        host: String,
        port: Int,
        user: String,
        authKind: Int,
        secret: String,
        passphrase: String?,
        policy: Int,
        fingerprint: String?,
        cols: Int,
        rows: Int,
    ): Long
    @JvmStatic external fun lastFingerprint(): String
    @JvmStatic external fun terminalFree(terminal: Long)
    @JvmStatic external fun terminalSendText(terminal: Long, text: String): Long
    @JvmStatic external fun terminalSendKey(terminal: Long, name: String): Long
    @JvmStatic external fun terminalSendControl(terminal: Long, codePoint: Int): Long
    @JvmStatic external fun terminalPaste(terminal: Long, text: String): Long
    @JvmStatic external fun terminalResize(terminal: Long, cols: Int, rows: Int): Long
    @JvmStatic external fun terminalScroll(terminal: Long, lines: Int): Long
    @JvmStatic external fun terminalWait(terminal: Long, seen: Long, timeoutMs: Int): Long
    @JvmStatic external fun terminalSnapshot(terminal: Long): String?
    @JvmStatic external fun terminalTakeClipboard(terminal: Long): String?
    /** [TIMEOUT] while the shell runs, [ENDED] once it ended (code[0] is its exit code, -1 for none). */
    @JvmStatic external fun terminalEnded(terminal: Long, code: IntArray): Long
    /** The process id (0 if unknown), or [ERROR] with the reason in lastError. */
    @JvmStatic external fun launch(session: Long, argvJson: String): Long

    // Account sign-in (windowcast.h). Failures leave the reason in lastError.
    /** The host's sign-in options as JSON, or null. */
    @JvmStatic external fun signInOptions(client: Long, address: String): String?
    /** A session, or 0. */
    @JvmStatic external fun connectAccount(client: Long, address: String, signInJson: String, acceptHost: String?): Long
    /** A browser sign-in (the page to open in url[0]), or 0. */
    @JvmStatic external fun oidcBrowserStart(client: Long, providerJson: String, url: Array<String?>): Long
    /** The ID token, or null; frees the sign-in either way. */
    @JvmStatic external fun oidcBrowserFinish(signIn: Long, timeoutMs: Int): String?
    /** A device sign-in (what to show the user, JSON, in shown[0]), or 0. */
    @JvmStatic external fun oidcDeviceStart(client: Long, providerJson: String, shown: Array<String?>): Long
    /** The ID token, or null with status[0] [TIMEOUT] (not yet) or [ERROR]. */
    @JvmStatic external fun oidcDeviceWait(signIn: Long, timeoutMs: Int, status: LongArray): String?
    @JvmStatic external fun oidcDeviceFree(signIn: Long)
}
