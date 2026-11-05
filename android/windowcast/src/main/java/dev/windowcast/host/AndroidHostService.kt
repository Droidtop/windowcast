package dev.windowcast.host

import android.content.Context
import android.os.RemoteException
import java.io.File
import kotlin.system.exitProcess

/**
 * windowcast's Android host as a Shizuku (or Sui) user service (Droidtop/tracker#615): Shizuku
 * starts this class in a process of the shell user, which may make displays other apps launch
 * onto and inject input into them. The embedding app reaches it through [AndroidHostConnection].
 *
 * Its identity and trusted devices live where the shell user may write,
 * `/data/local/tmp/windowcast-host-<package>`, since the app's own folder is not the shell's.
 */
class AndroidHostService(private val context: Context) : IAndroidHost.Stub() {
    private var host: AndroidHost? = null

    override fun start(listen: String, pairing: Boolean): String = synchronized(this) {
        host?.close()
        val dir = File("/data/local/tmp", "windowcast-host-${context.packageName}").apply { mkdirs() }
        val started = try {
            AndroidHost(context, dir, listen, pairing)
        } catch (e: java.io.IOException) {
            throw RemoteException(e.message)
        }
        host = started
        started.id
    }

    override fun openApp(packageName: String, width: Int, height: Int, dpi: Int): Long {
        val running = synchronized(this) { host } ?: throw RemoteException("the host is not started")
        return try {
            running.openApp(packageName, width, height, dpi)
        } catch (e: java.io.IOException) {
            throw RemoteException(e.message)
        }
    }

    override fun closeApp(window: Long) {
        synchronized(this) { host }?.closeApp(window)
    }

    override fun destroy() {
        synchronized(this) {
            host?.close()
            host = null
        }
        // Shizuku's contract: the service's process ends with it.
        exitProcess(0)
    }
}
