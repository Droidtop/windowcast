package dev.windowcast

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class SessionHolderTest {
    /** A session that writes what is done to it into [log], in order. */
    private class Fake(val name: String, val log: MutableList<String>) : HostedSession {
        val queue = LinkedBlockingQueue<Event>()
        @Volatile override var isOpen = true

        override fun nextEvent(timeoutMs: Int): Event? = queue.poll(timeoutMs.toLong(), TimeUnit.MILLISECONDS)
        override fun acceptPictures(on: Boolean) { log += "$name pictures $on" }
        override fun requestWindows() { log += "$name windows" }
        override fun close() { isOpen = false; log += "$name closed" }
    }

    private val ignore = object : SessionListener {}

    @Test
    fun aNewSessionReplacesTheOldInOrder() {
        val log = java.util.Collections.synchronizedList(mutableListOf<String>())
        val holder = SessionHolder<Fake>()
        val first = holder.start(ignore, open = { log += "open first"; Fake("first", log) })
        assertSame(first, holder.session)
        val second = holder.start(
            ignore,
            beforeClose = { log += "before close" },
            opened = { log += "opened ${it.name}" },
            open = { log += "open second"; Fake("second", log) },
        )
        assertSame(second, holder.session)
        assertEquals(
            listOf(
                "open first", "first pictures true", "first windows",
                "before close", "first closed",
                "open second", "second pictures true", "opened second", "second windows",
            ),
            log,
        )
        holder.end()
    }

    @Test
    fun aFailedOpenLeavesNothingRunning() {
        val log = java.util.Collections.synchronizedList(mutableListOf<String>())
        val holder = SessionHolder<Fake>()
        val first = holder.start(ignore, open = { Fake("first", log) })
        val failed = runCatching { holder.start(ignore, open = { throw java.io.IOException("refused") }) }
        assertTrue(failed.exceptionOrNull() is java.io.IOException)
        assertNull(holder.session)
        assertTrue(!first.isOpen)
    }

    @Test
    fun releaseHandsTheSessionOnWithoutClosingIt() {
        val log = java.util.Collections.synchronizedList(mutableListOf<String>())
        val holder = SessionHolder<Fake>()
        val s = holder.start(ignore, open = { Fake("kept", log) })
        assertSame(s, holder.release())
        assertNull(holder.session)
        assertTrue(s.isOpen)
        // Adopted again (an activity recreated): events flow once more.
        val heard = java.util.concurrent.CountDownLatch(1)
        holder.adopt(s, object : SessionListener {
            override fun onClipboard(text: String) { heard.countDown() }
        })
        s.queue += Event.Clipboard("x")
        assertTrue(heard.await(2, TimeUnit.SECONDS))
        holder.end()
        assertTrue(!s.isOpen)
    }
}
