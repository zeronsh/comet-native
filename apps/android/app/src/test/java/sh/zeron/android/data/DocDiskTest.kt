package sh.zeron.android.data

import kotlinx.coroutines.test.runTest
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import sh.zeron.android.loro.FakeLoroDoc
import java.io.File

class DocDiskTest {
    private lateinit var dir: File

    @Before
    fun setUp() {
        dir = createTempDir()
        DocDisk.testDirectory = dir
    }

    @After
    fun tearDown() {
        DocDisk.testDirectory = null
        dir.deleteRecursively()
    }

    @Test
    fun chat2SnapshotRoundTripsDocAndCursor() = runTest {
        val doc = FakeLoroDoc("""{"messages":[{"id":"m1","role":"user"}]}""")
        DocDisk.saveChat2(doc, "chat-1", cursor = 42)

        val fresh = FakeLoroDoc("{}")
        val cursor = DocDisk.loadChat2(fresh, "chat-1")
        assertEquals(42L, cursor)
        // The snapshot's bytes imported into the fresh doc.
        assert(fresh.json.contains("m1"))
    }

    @Test
    fun absentSnapshotReadsNull() = runTest {
        assertNull(DocDisk.loadChat2(FakeLoroDoc("{}"), "never-saved"))
    }
}
