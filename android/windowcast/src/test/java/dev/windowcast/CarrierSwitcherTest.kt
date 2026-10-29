package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class CarrierSwitcherTest {
    private var host: String? = "host-a"
    private val switcher = CarrierSwitcher<String> { host }

    @Test
    fun aSwitchIsPendingFromTheHostsStartUntilItsFirstPicture() {
        assertNull(switcher.pending)
        val p = switcher.begin(3, "Rdp")
        assertSame(p, switcher.pending)
        assertTrue(switcher.isPending(3))
        assertFalse(switcher.isPending(2))
        assertTrue(switcher.finish(p))
        assertNull(switcher.pending)
    }

    @Test
    fun aFinishIsHonouredOnceAndOnlyForTheSwitchUnderWay() {
        val old = switcher.begin(1, "Rdp")
        val newer = switcher.begin(2, "Native")
        assertFalse(switcher.finish(old))
        assertSame(newer, switcher.pending)
        assertTrue(switcher.finish(newer))
        assertFalse(switcher.finish(newer))
    }

    @Test
    fun cancelGivesBackWhatWasStartedSoTheClientCanStopIt() {
        val p = switcher.begin(1, "Rdp")
        p.renderer = "renderer"
        val cancelled = switcher.cancel()
        assertSame(p, cancelled)
        assertEquals("renderer", cancelled!!.renderer)
        assertNull(switcher.pending)
        assertNull(switcher.cancel())
    }

    @Test
    fun aRefusedSwitchIsNotOfferedAgainForThatWindowAndHost() {
        assertTrue(switcher.offered(7, "Rdp"))
        switcher.refuse(7, "Rdp")
        assertFalse(switcher.offered(7, "Rdp"))
        assertTrue("the other carrier is still offered", switcher.offered(7, "Native"))
        assertTrue("another window is still offered", switcher.offered(8, "Rdp"))
        host = "host-b"
        assertTrue("another host is still offered", switcher.offered(7, "Rdp"))
        host = "host-a"
        assertFalse(switcher.offered(7, "Rdp"))
    }

    @Test
    fun withNoWindowEverythingIsOffered() {
        switcher.refuse(7, "Rdp")
        assertTrue(switcher.offered(null, "Rdp"))
    }
}
