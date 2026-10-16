package dev.windowcast.viewer

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ConnectRequestTest {
    private val id = "0123456789abcdef".repeat(4)

    @Test
    fun aLinkNamesTheHostItsAddressAndAWindow() {
        val request = ConnectRequest.fromLink(
            "windowcast://connect?host=${id.uppercase()}&address=192.168.1.20:47100&window=Hollow%20Knight",
        )
        assertEquals(ConnectRequest(id, "192.168.1.20:47100", "Hollow Knight"), request)
    }

    @Test
    fun theWindowIsOptionalAndAHostNameOrIpv6Works() {
        assertEquals(
            ConnectRequest(id, "pc.local:47100", null),
            ConnectRequest.fromLink("windowcast://connect?address=pc.local%3A47100&host=$id"),
        )
        assertEquals(
            ConnectRequest(id, "[fe80::1]:47100", null),
            ConnectRequest.of(id, "[fe80::1]:47100", "  "),
        )
    }

    @Test
    fun anythingElseIsRefused() {
        for (link in listOf(
            "windowcast://connect?host=$id",
            "windowcast://connect?address=pc:47100",
            "windowcast://pair?host=$id&address=pc:47100",
            "https://connect?host=$id&address=pc:47100",
            "windowcast://connect?host=${id.dropLast(1)}&address=pc:47100",
            "windowcast://connect?host=${id.dropLast(1)}g&address=pc:47100",
            "windowcast://connect?host=$id&address=pc",
            "windowcast://connect?host=$id&address=pc:0",
            "windowcast://connect?host=$id&address=pc:70000",
            "windowcast://connect?host=$id&address=user%40pc:47100",
            "windowcast://connect?host=$id&address=pc:47100%2Fpath",
            "windowcast://connect?host=$id&address=%",
            "not a link",
        )) {
            assertNull(link, ConnectRequest.fromLink(link))
        }
        assertNull(ConnectRequest.of(null, "pc:47100", null))
        assertNull(ConnectRequest.of(id, null, null))
    }
}
