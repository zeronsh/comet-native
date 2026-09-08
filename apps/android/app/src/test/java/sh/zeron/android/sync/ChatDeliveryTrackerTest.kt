package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.runTest
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import sh.zeron.android.data.ChatRow
import sh.zeron.android.data.DocDisk
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Workspace badge derivation (iOS HomeView sendBadge): the tracker scans each
 * chat's persisted chat2 snapshot for the oldest own pending command whose
 * user message never landed, then derives Queued/Failed from connectivity and
 * the 2-minute grace. The snapshot file + injected docReader stand in for the
 * Loro import — the wire format (magic + cursor + snapshot) is DocDisk's.
 */
class ChatDeliveryTrackerTest {
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

    private val offline = MutableStateFlow(false)
    private val presence = MutableStateFlow<Map<String, Long>>(emptyMap())
    private val connected = MutableStateFlow(true)

    private val chat = ChatRow(
        id = "c1", title = "T", archived = false, spaceId = "s1", deviceId = "desktop-1",
    )

    private fun writeSnapshot(chatId: String, json: String) {
        val file = File(dir, "c2_${chatId.replace("/", "_")}.loro")
        val cursor = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(42).array()
        file.writeBytes("C2SNAP01".toByteArray() + cursor + json.toByteArray())
    }

    private fun tracker(scope: CoroutineScope): ChatDeliveryTracker = ChatDeliveryTracker(
        scope = scope,
        offline = offline,
        presence = presence,
        registryConnected = connected,
    ) { bytes -> String(bytes, Charsets.UTF_8) }

    /** A pending own run command whose user message never landed. */
    private fun pendingCommand(issuedAt: Long = 1_000L, messageId: String = "m1") = """
        {"messages":[{"id":"m2","role":"user"}],
         "commands":[{"issuedBy":"android","kind":"run","status":"pending",
           "issuedAt":$issuedAt,"payload":{"messageId":"$messageId"}}]}
    """.trimIndent()

    @Test
    fun queuedBadgeWhenHostIsDark() = runTest {
        writeSnapshot("c1", pendingCommand())
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll() // await the IO scan deterministically
        tracker.recompute()

        // Host presence unknown → degraded path → Queued (not yet Failed).
        assertEquals(SendState.Queued, tracker.badges.value["c1"])
    }

    @Test
    fun failedBadgeAfterTheTwoMinuteGrace() = runTest {
        val stale = System.currentTimeMillis() - UNDELIVERED_GRACE_MS - 1_000
        writeSnapshot("c1", pendingCommand(issuedAt = stale))
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll()
        tracker.recompute()

        // Unadopted past the grace — Failed wins over the degraded-path Queued.
        assertEquals(SendState.Failed, tracker.badges.value["c1"])
    }

    @Test
    fun healthyInFlightReadsSendingWithinGrace() = runTest {
        writeSnapshot("c1", pendingCommand(issuedAt = System.currentTimeMillis() - 5_000))
        presence.value = mapOf("desktop-1" to System.currentTimeMillis())
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll()
        tracker.recompute()

        assertEquals(SendState.Sending, tracker.badges.value["c1"])
    }

    @Test
    fun noBadgeOnceTheMessageLanded() = runTest {
        // The command's message id now exists in messages — nothing pending.
        writeSnapshot("c1", pendingCommand(messageId = "m2"))
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll()
        tracker.recompute()

        assertNull(tracker.badges.value["c1"])
    }

    @Test
    fun liveOverrideWinsForTheOpenSession() = runTest {
        writeSnapshot("c1", pendingCommand(issuedAt = System.currentTimeMillis() - 5_000))
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll()
        tracker.recompute()

        // The open session's real pendingSends truth replaces the doc-derived one.
        tracker.setLive("c1", SendState.Failed)
        assertEquals(SendState.Failed, tracker.badges.value["c1"])
        tracker.setLive("c1", null)
        assertEquals(SendState.Sending, tracker.badges.value["c1"])
    }

    @Test
    fun expiredCommandsDoNotCountAsPending() = runTest {
        // expiresAt in the past: the host gave up on it — no badge.
        val json = """
            {"messages":[{"id":"m2","role":"user"}],
             "commands":[{"issuedBy":"android","kind":"run","status":"pending",
               "issuedAt":1000,"expiresAt":${System.currentTimeMillis() - 1_000},
               "payload":{"messageId":"m1"}}]}
        """.trimIndent()
        writeSnapshot("c1", json)
        val tracker = tracker(CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        tracker.setChats(listOf(chat))
        tracker.rescanAll()
        tracker.recompute()

        assertNull(tracker.badges.value["c1"])
    }
}
