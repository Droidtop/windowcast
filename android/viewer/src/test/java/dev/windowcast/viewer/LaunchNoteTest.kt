package dev.windowcast.viewer

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class LaunchNoteTest {
    @Test
    fun theNoteSurvivesEveryListThatArrivesWhileItIsCurrent() {
        val note = LaunchNote(keepMs = 10_000)
        note.set("Started notepad.exe (process 4242)", nowMs = 1_000)
        assertEquals("Started notepad.exe (process 4242). 2 windows. Tap one to watch it.", note.windowsLine(2, nowMs = 1_100))
        assertEquals("Started notepad.exe (process 4242). 3 windows. Tap one to watch it.", note.windowsLine(3, nowMs = 3_000))
        assertEquals("Started notepad.exe (process 4242). 3 windows. Tap one to watch it.", note.windowsLine(3, nowMs = 10_999))
    }

    @Test
    fun theNoteEndsByItself() {
        val note = LaunchNote(keepMs = 10_000)
        note.set("Started notepad.exe", nowMs = 0)
        assertNull(note.current(10_000))
        assertEquals("3 windows. Tap one to watch it.", note.windowsLine(3, nowMs = 10_000))
    }

    @Test
    fun withNoNoteTheLineIsJustTheCount() {
        assertEquals("1 windows. Tap one to watch it.", LaunchNote().windowsLine(1, nowMs = 5))
    }

    @Test
    fun aNewLaunchReplacesTheNoteAndRestartsTheTime() {
        val note = LaunchNote(keepMs = 10_000)
        note.set("Started a", nowMs = 0)
        note.set("Started b", nowMs = 9_000)
        assertEquals("Started b", note.current(15_000))
    }
}
