package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Test

class QualityTest {
    @Test
    fun aHeldStreamSaysWhatItIsHeldToAndTheNetwork() {
        val q = Quality(targetKbps = 4000, sentKbps = 3200, fps = 60, width = 1280, height = 720, lossPercent = 0.44f, rttMs = 12)
        assertEquals("held to 4.0 Mbit/s, 1280x720 at 60 fps, sending 3.2 Mbit/s; loss 0.4%, round trip 12 ms", q.describe())
    }

    @Test
    fun aStreamNothingAdaptsToSaysAsSetAndLeavesOutAnUnknownRoundTrip() {
        val q = Quality(targetKbps = null, sentKbps = 800, fps = 30, width = 640, height = 480, lossPercent = 0f, rttMs = null)
        assertEquals("as set, 640x480 at 30 fps, sending 0.8 Mbit/s; loss 0.0%", q.describe())
    }

    @Test
    fun theTextDoesNotFollowTheDevicesLocale() {
        val before = java.util.Locale.getDefault()
        try {
            java.util.Locale.setDefault(java.util.Locale.GERMANY)
            val q = Quality(2500, 2500, 24, 100, 100, 1.5f, 5)
            assertEquals("held to 2.5 Mbit/s, 100x100 at 24 fps, sending 2.5 Mbit/s; loss 1.5%, round trip 5 ms", q.describe())
        } finally {
            java.util.Locale.setDefault(before)
        }
    }
}
