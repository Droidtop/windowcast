package dev.windowcast.host

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.os.IBinder
import rikka.shizuku.Shizuku

/**
 * Starts windowcast's Android host as the shell user through Shizuku or Sui and hands back its
 * binder (Droidtop/tracker#615). The one call an embedding app (droidtop) makes; everything
 * after it is [IAndroidHost]: start, open an app as a window, close it.
 *
 * Shizuku's client library is the embedding app's (`dev.rikka.shizuku:api`, which droidtop
 * already ships); this library only compiles against it. The app checks Shizuku is running and
 * granted before calling, as it does for every elevated call.
 */
class AndroidHostConnection(context: Context, private val ready: (IAndroidHost?) -> Unit) {
    private val args = Shizuku.UserServiceArgs(ComponentName(context.packageName, AndroidHostService::class.java.name))
        .processNameSuffix("windowcast-host")
        .daemon(false)
        .debuggable(false)
        .version(SERVICE_VERSION)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder?) {
            ready(binder?.takeIf { it.pingBinder() }?.let { IAndroidHost.Stub.asInterface(it) })
        }

        override fun onServiceDisconnected(name: ComponentName) {
            ready(null)
        }
    }

    /** Asks Shizuku for the host; [ready] gets it, or null when it goes. */
    fun bind() {
        Shizuku.bindUserService(args, connection)
    }

    /** Lets the host go: Shizuku ends its process ([IAndroidHost.destroy]). */
    fun unbind() {
        Shizuku.unbindUserService(args, connection, true)
    }

    private companion object {
        /** Raised when the service's code changes, so Shizuku restarts a running old one. */
        const val SERVICE_VERSION = 1
    }
}
