package sh.zeron.android.data

import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

class DeviceIdStoreTest {
    @Test fun stableAcrossReads() = runTest {
        val s = InMemoryDeviceIdStore()
        val a = s.getOrCreate()
        val b = s.getOrCreate()
        assertEquals(a, b)
    }
    @Test fun resetGeneratesNew() = runTest {
        val s = InMemoryDeviceIdStore()
        val a = s.getOrCreate()
        s.reset()
        val b = s.getOrCreate()
        assertNotEquals(a, b)
    }
}
