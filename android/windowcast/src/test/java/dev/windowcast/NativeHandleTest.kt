package dev.windowcast

import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NativeHandleTest {
    private val freed = mutableListOf<Long>()
    private fun handle() = NativeHandle(7L) { synchronized(freed) { freed += it } }

    @Test
    fun callsRunWithTheHandleUntilClosed() {
        val h = handle()
        assertEquals(7L, h.use(0L) { it })
        assertTrue(h.isOpen)
        h.close()
        assertFalse(h.isOpen)
        assertEquals(-1L, h.use(-1L) { error("must not run on a closed handle") })
        assertEquals(listOf(7L), freed)
    }

    @Test
    fun closeTwiceFreesOnce() {
        val h = handle()
        h.close()
        h.close()
        assertEquals(listOf(7L), freed)
    }

    @Test
    fun closeDuringACallWaitsForItsEndWithoutBlocking() {
        val h = handle()
        val inside = CountDownLatch(1)
        val leave = CountDownLatch(1)
        val result = AtomicInteger()
        val caller = Thread {
            h.use(0) {
                inside.countDown()
                leave.await(5, TimeUnit.SECONDS)
                // Still the live handle: nothing freed under us.
                result.set(if (synchronized(freed) { freed.isEmpty() }) 1 else 2)
            }
        }
        caller.start()
        assertTrue(inside.await(5, TimeUnit.SECONDS))
        h.close() // returns at once, although a call is running
        assertEquals(emptyList<Long>(), synchronized(freed) { freed.toList() })
        assertEquals(-1, h.use(-1) { 0 })
        leave.countDown()
        caller.join(5000)
        assertEquals(1, result.get())
        assertEquals(listOf(7L), freed)
    }

    @Test
    fun manyConcurrentCallsAndOneCloseFreeOnce() {
        val h = handle()
        val threads = (1..8).map {
            Thread { repeat(2000) { h.use(0) { 1 } } }
        }
        threads.forEach { it.start() }
        h.close()
        threads.forEach { it.join(5000) }
        assertEquals(listOf(7L), freed)
    }
}
