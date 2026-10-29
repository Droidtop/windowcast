package dev.windowcast

import java.net.URI
import java.net.URLDecoder

/**
 * Another app asking the viewer to open a host's stream (docs/ANDROID-VIEWER.md, "Opening a
 * host from another app"): the host's device id, where it listens, and optionally the title of
 * the window to watch. The viewer only resumes with hosts it has paired with, and only with the
 * one this id names; a request never pairs. Plain Kotlin, so it is tested off the device.
 */
data class ConnectRequest(val hostId: String, val address: String, val window: String?) {
    companion object {
        const val ACTION = "dev.windowcast.viewer.action.CONNECT"
        const val EXTRA_HOST_ID = "dev.windowcast.viewer.extra.HOST_ID"
        const val EXTRA_ADDRESS = "dev.windowcast.viewer.extra.ADDRESS"
        const val EXTRA_WINDOW = "dev.windowcast.viewer.extra.WINDOW"
        const val SCHEME = "windowcast"
        const val LINK_HOST = "connect"

        /** A device id: the host's public key, 32 bytes in hex. */
        private val DEVICE_ID = Regex("[0-9a-f]{64}")

        /** Checks and normalises the three values; `null` when the host id or address is unusable. */
        fun of(hostId: String?, address: String?, window: String?): ConnectRequest? {
            val id = hostId?.trim()?.lowercase() ?: return null
            if (!DEVICE_ID.matches(id)) return null
            val where = address?.trim() ?: return null
            if (!validAddress(where)) return null
            return ConnectRequest(id, where, window?.trim()?.takeIf { it.isNotEmpty() })
        }

        /** `windowcast://connect?host=<device id>&address=<host:port>[&window=<title>]`. */
        fun fromLink(link: String): ConnectRequest? {
            val uri = try {
                URI(link)
            } catch (e: Exception) {
                return null
            }
            if (!SCHEME.equals(uri.scheme, ignoreCase = true) || uri.host != LINK_HOST) return null
            val query = (uri.rawQuery ?: return null).split('&').mapNotNull { pair ->
                val at = pair.indexOf('=')
                if (at <= 0) return@mapNotNull null
                try {
                    URLDecoder.decode(pair.substring(0, at), "UTF-8") to
                        URLDecoder.decode(pair.substring(at + 1), "UTF-8")
                } catch (e: IllegalArgumentException) {
                    null
                }
            }.toMap()
            return of(query["host"], query["address"], query["window"])
        }

        /** `HOST:PORT`, an IPv6 host in brackets, a port from 1 to 65535, nothing else. */
        private fun validAddress(address: String): Boolean {
            if (address.any { it.isWhitespace() || it in "/@?#" }) return false
            val uri = try {
                URI("tcp://$address")
            } catch (e: Exception) {
                return false
            }
            return !uri.host.isNullOrEmpty() && uri.port in 1..65535 && uri.rawPath.isNullOrEmpty()
        }
    }
}
