package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class EventPumpTest {
    /** A source that hands out what a test queued, then waits as a real session does. */
    private class FakeSource : EventSource {
        val queue = LinkedBlockingQueue<Event>()
        @Volatile override var isOpen = true

        override fun nextEvent(timeoutMs: Int): Event? = queue.poll(timeoutMs.toLong(), TimeUnit.MILLISECONDS)
    }

    private class Recorder : SessionListener {
        val seen = java.util.Collections.synchronizedList(mutableListOf<String>())
        val closed = CountDownLatch(1)
        override fun onWindows(windows: List<WindowInfo>) { seen += "windows:${windows.size}" }
        override fun onStreamStarted(event: Event.StreamStarted) { seen += "started:${event.window}" }
        override fun onStreamRefused(event: Event.StreamRefused) { seen += "refused:${event.window}:${event.reason}" }
        override fun onClipboard(text: String) { seen += "clipboard:$text" }
        override fun onClosed() { seen += "closed"; closed.countDown() }
    }

    @Test
    fun eventsArriveInOrderAndTheCloseEndsThePump() {
        val source = FakeSource()
        val listener = Recorder()
        val pump = EventPump(source, listener, pollMs = 20).start()
        source.queue += Event.Windows(emptyList())
        source.queue += Event.StreamStarted(7, "native", null)
        source.queue += Event.StreamRefused(8, "too big")
        source.queue += Event.Clipboard("hi")
        source.queue += Event.Closed
        source.queue += Event.StreamStarted(9, "native", null) // after the close: never seen
        assertTrue(listener.closed.await(2, TimeUnit.SECONDS))
        pump.close()
        assertEquals(listOf("windows:0", "started:7", "refused:8:too big", "clipboard:hi", "closed"), listener.seen.toList())
    }

    @Test
    fun anEventNoMethodIsOverriddenForIsIgnored() {
        val source = FakeSource()
        val listener = Recorder()
        val pump = EventPump(source, listener, pollMs = 20).start()
        source.queue += Event.WindowFocused(1)
        source.queue += Event.Cursor(1, 0f, 0f, true, 0)
        source.queue += Event.Closed
        assertTrue(listener.closed.await(2, TimeUnit.SECONDS))
        pump.close()
        assertEquals(listOf("closed"), listener.seen.toList())
    }

    @Test
    fun closingThePumpFirstSilencesTheClosedCallback() {
        val source = FakeSource()
        val listener = Recorder()
        val pump = EventPump(source, listener, pollMs = 20).start()
        pump.close()
        source.queue += Event.Closed
        assertFalse(listener.closed.await(200, TimeUnit.MILLISECONDS))
        assertEquals(emptyList<String>(), listener.seen.toList())
    }

    @Test
    fun theThreadEndsWhenTheSessionDoes() {
        val source = FakeSource()
        val listener = Recorder()
        val pump = EventPump(source, listener, pollMs = 20).start()
        source.isOpen = false
        pump.close()
        assertEquals(emptyList<String>(), listener.seen.toList())
    }

    @Test(expected = IllegalStateException::class)
    fun aPumpStartsOnce() {
        val pump = EventPump(FakeSource(), Recorder(), pollMs = 20).start()
        try {
            pump.start()
        } finally {
            pump.close()
        }
    }

    @Test
    fun aListenerMayClosePumpFromItsOwnThread() {
        val source = FakeSource()
        val done = CountDownLatch(1)
        lateinit var pump: EventPump
        pump = EventPump(source, object : SessionListener {
            override fun onStreamStopped(event: Event.StreamStopped) {
                pump.close()
                done.countDown()
            }
        }, pollMs = 20).start()
        source.queue += Event.StreamStopped(1)
        assertTrue(done.await(2, TimeUnit.SECONDS))
    }
}
