package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import sh.zeron.android.data.SessionAdapter
import sh.zeron.android.loro.FakeLoroDoc
import sh.zeron.android.protocol.Chat2Codec

class SessionRepositoryTest {
    private class RecordingWs : FakeWebSocketTransport() {
        val sent = mutableListOf<ByteArray>()
        override suspend fun send(message: WsMessage) {
            if (message is WsMessage.Binary) sent += message.bytes
        }
    }

    private fun connectSync(sync: ChatSync, ws: RecordingWs) {
        ws.emit(WsMessage.Connected)
        val state = Chat2Codec.encode(
            Chat2Codec.STATE,
            JSONObject()
                .put("headSeq", 0L)
                .put("seqFloor", 0L)
                .put("checkpointSeq", 0L)
                .put("checkpointSize", 0L)
                .put("rowCount", 0)
                .put("rowBytes", 0),
            payload = byteArrayOf(1),
        )
        ws.emit(WsMessage.Binary(state))
    }

    @Test
    fun sendStateDerivesSendingQueuedFailed() = runTest {
        val ws = RecordingWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val doc = FakeLoroDoc("{}")
        val sync = ChatSync("chat-1", ws, FakeHttpTransport(), doc, scope)
        val repo = SessionRepository("chat-1", doc, SessionAdapter(doc), sync, scope)
        repo.observe()
        sync.start(0, "dev", "wss://edge.test/chat2/chat-1/ws")
        connectSync(sync, ws)
        advanceUntilIdle()

        repo.sendPrompt("hello", "claude-code", "claude-fable-5")
        advanceUntilIdle()

        val now = System.currentTimeMillis()
        // Healthy connected + host online → Sending.
        assertEquals(SendState.Sending, repo.sendState(now, offline = false, hostOnline = true))
        // OS path down → Queued.
        assertEquals(SendState.Queued, repo.sendState(now, offline = true, hostOnline = true))
        // Host presence dark → Queued.
        assertEquals(SendState.Queued, repo.sendState(now, offline = false, hostOnline = false))
        // Unadopted past the 2-minute grace → Failed.
        assertEquals(SendState.Failed, repo.sendState(now + UNDELIVERED_GRACE_MS + 1, offline = false, hostOnline = true))
    }

    @Test
    fun retryReissuesDeadSends() = runTest {
        // A doc with a REJECTED run command whose user message never landed.
        val json = """{"commands":[{"id":"c1","kind":"run","issuedBy":"android","status":"rejected",
            "issuedAt":1000,"expiresAt":9999999999999,
            "payload":{"kind":"run","messageId":"m-dead",
              "request":{"prompt":"hello","harness":"claude-code"}}}]}"""
        val ws = RecordingWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val doc = FakeLoroDoc(json)
        val sync = ChatSync("chat-1", ws, FakeHttpTransport(), doc, scope)
        val repo = SessionRepository("chat-1", doc, SessionAdapter(doc), sync, scope)
        repo.observe()
        sync.start(0, "dev", "wss://edge.test/chat2/chat-1/ws")
        connectSync(sync, ws)
        advanceUntilIdle()

        val pushesBefore = ws.sent.count { it.isNotEmpty() && Chat2Codec.decode(it)?.kind == Chat2Codec.PUSH }
        repo.retryDelivery()
        advanceUntilIdle()

        // The dead command was re-issued: a fresh push carrying the same
        // messageId went out on the live room.
        val pushesAfter = ws.sent.count { it.isNotEmpty() && Chat2Codec.decode(it)?.kind == Chat2Codec.PUSH }
        assertTrue("expected a re-issue push, had $pushesBefore -> $pushesAfter", pushesAfter > pushesBefore)
        val reissuePayloads = ws.sent
            .mapNotNull { Chat2Codec.decode(it) }
            .filter { it.kind == Chat2Codec.PUSH }
            .map { String(it.payload) }
        assertTrue("re-issue must carry the dead messageId", reissuePayloads.any { "m-dead" in it })
    }
}
