package dev.windowcast

import java.io.IOException
import org.json.JSONArray

/**
 * A host heard on the local network (docs/DISCOVERY.md). Only a hint of where to connect: it is not
 * trusted until [WindowcastClient.connect] proves the identity [deviceId] names, and a host is paired
 * or resumed exactly as if its [address] had been typed.
 */
data class DiscoveredHost(
    /** The host's device id, 64 hex digits (what a session reports as its host id). */
    val deviceId: String,
    /** The name its owner gave it. */
    val name: String,
    /** `HOST:PORT` to connect to. */
    val address: String,
)

/** Finding hosts on the local network and waking one that sleeps (Droidtop/tracker#113). */
object HostDiscovery {
    /**
     * Listens for [windowMs] (at most 30 s) and lists the hosts that announced themselves; hosts
     * announce only when their application turns it on. Blocks for the whole window: call it off the
     * main thread. Only one listener per device can hold the port at a time.
     */
    @Throws(IOException::class)
    fun find(windowMs: Int = 4_000): List<DiscoveredHost> {
        val json = Native.discoverHosts(windowMs.coerceIn(0, 30_000)) ?: throw IOException(Native.lastError())
        return parse(json)
    }

    /**
     * Wakes a sleeping computer with Wake-on-LAN: broadcasts the magic packet for [mac]
     * (`aa:bb:cc:dd:ee:ff`) on the local network, and also to [directed] (the subnet's broadcast
     * address, e.g. `192.168.1.255`) when given. The computer's network card has to be set to wake
     * on it.
     */
    @Throws(IOException::class)
    fun wake(mac: String, directed: String? = null) {
        if (Native.wakeHost(mac, directed) != 0L) throw IOException(Native.lastError())
    }

    internal fun parse(json: String): List<DiscoveredHost> {
        val list = JSONArray(json)
        return (0 until list.length()).map { i ->
            val host = list.getJSONObject(i)
            DiscoveredHost(host.getString("device_id"), host.getString("name"), host.getString("address"))
        }
    }
}
