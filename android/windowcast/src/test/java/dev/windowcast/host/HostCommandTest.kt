package dev.windowcast.host

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class HostCommandTest {
    @Test fun theHostsCommandsAreRead() {
        assertEquals(HostCommand.Open(4, "h264"), HostCommand.parse("""{"command":"open","window":4,"codec":"h264"}"""))
        assertEquals(HostCommand.Close(4), HostCommand.parse("""{"command":"close","window":4}"""))
        assertEquals(HostCommand.OpenAudio(7), HostCommand.parse("""{"command":"open_audio","window":7}"""))
        assertEquals(HostCommand.CloseAudio(7), HostCommand.parse("""{"command":"close_audio","window":7}"""))
        assertEquals(HostCommand.Keyframe(7), HostCommand.parse("""{"command":"keyframe","window":7}"""))
        assertEquals(HostCommand.Bitrate(7, 2000000), HostCommand.parse("""{"command":"bitrate","window":7,"bitrate":2000000}"""))
        assertNull(HostCommand.parse("""{"command":"dance","window":7}"""))
        assertEquals(
            HostCommand.Input(4, """{"PointerMove":{"window":4,"x":0.5,"y":0.25}}"""),
            HostCommand.parse("""{"command":"input","window":4,"event":{"PointerMove":{"window":4,"x":0.5,"y":0.25}}}"""),
        )
        assertNull(HostCommand.parse("""{"command":"open","window":4}"""))
    }

    @Test fun codecsHaveTheirMediaCodecTypes() {
        assertEquals("video/avc", HostCommand.mime("h264"))
        assertEquals("video/hevc", HostCommand.mime("h265"))
        assertEquals("video/av01", HostCommand.mime("av1"))
        assertNull(HostCommand.mime("vp8"))
    }

    @Test fun theWindowListIsTheHostsJson() {
        val json = windowsJson(listOf(HostApp(1, "org.notes", "My \"Notes\"", 1280, 720)))
        assertEquals("""[{"id":1,"title":"My \"Notes\"","package":"org.notes","width":1280,"height":720}]""", json)
    }
}
