package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import sh.zeron.android.loro.FakeLoroDoc
import sh.zeron.android.protocol.Chat2Codec

class ChatSyncTest {
    private class RecordingWs : FakeWebSocketTransport() {
        val sent = mutableListOf<ByteArray>()
        override suspend fun send(message: WsMessage) {
            if (message is WsMessage.Binary) sent += message.bytes
        }
    }

    private class RecordingDoc : FakeLoroDoc() {
        val imported = mutableListOf<ByteArray>()
        override suspend fun importBytes(bytes: ByteArray) {
            super.importBytes(bytes)
            imported += bytes
        }
    }

    private fun state(checkpointSize: Long, checkpointSeq: Long, headSeq: Long) = Chat2Codec.encode(
        Chat2Codec.STATE,
        JSONObject()
            .put("headSeq", headSeq)
            .put("seqFloor", 0)
            .put("checkpointSeq", checkpointSeq)
            .put("checkpointSize", checkpointSize)
            .put("rowCount", 0)
            .put("rowBytes", 0),
        // Frontier payload — a fresh, per-open doc can never contain it.
        payload = byteArrayOf(1, 2, 3),
    )

    private fun row(seq: Long) = Chat2Codec.encode(
        Chat2Codec.ROW,
        JSONObject().put("seq", seq).put("device", "d").put("batchId", "b$seq"),
        payload = byteArrayOf(9, 9),
    )

    private fun newSync(ws: RecordingWs, doc: RecordingDoc, http: HttpTransport, testScope: CoroutineScope) =
        ChatSync("chat-1", ws, http, doc, testScope)

    @Test
    fun compactedRoomFetchesCheckpointBeforeRows() = runTest {
        val ws = RecordingWs()
        val doc = RecordingDoc()
        val checkpointBytes = "checkpoint-blob".toByteArray()
        var checkpointRequests = 0
        val http = FakeHttpTransport { url, _, _ ->
            checkpointRequests += 1
            assertTrue(url.startsWith("https://edge.test/chat2/chat-1/checkpoint"))
            HttpResponse(200, checkpointBytes)
        }
        val sync = newSync(ws, doc, http, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        sync.start(
            cursor = 0,
            deviceId = "dev",
            url = "wss://edge.test/chat2/chat-1/ws?token=t&device=dev",
            checkpointUrl = "https://edge.test/chat2/chat-1/checkpoint?token=t",
        )

        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Binary(state(checkpointSize = 1000, checkpointSeq = 5, headSeq = 10)))

        assertEquals("checkpoint fetched exactly once", 1, checkpointRequests)
        assertTrue(sync.connected.value)
        assertFalse("checkpoint imported → 'older messages' flag clears", sync.checkpointPending.value)
        assertEquals("checkpoint bytes land in the doc before any row", 1, doc.imported.size)
        assertArrayEquals(checkpointBytes, doc.imported[0])

        // rowsReq went out after the checkpoint, from the raised cursor.
        val rowsReq = ws.sent.last { Chat2Codec.decode(it)?.kind == Chat2Codec.ROWS_REQ }
        assertEquals(5L, Chat2Codec.decode(rowsReq)?.header?.optLong("after"))

        // A live row after the checkpoint imports normally and walks the cursor.
        ws.emit(WsMessage.Binary(row(seq = 6)))
        assertEquals(2, doc.imported.size)
        assertArrayEquals(byteArrayOf(9, 9), doc.imported[1])
    }

    @Test
    fun noCheckpointStreamsRowsFromTheCursor() = runTest {
        val ws = RecordingWs()
        val doc = RecordingDoc()
        var checkpointRequests = 0
        val http = FakeHttpTransport { url, _, _ ->
            checkpointRequests += 1
            HttpResponse(200, ByteArray(0))
        }
        val sync = newSync(ws, doc, http, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        sync.start(cursor = 0, deviceId = "dev", url = "wss://edge.test/chat2/chat-1/ws")

        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Binary(state(checkpointSize = 0, checkpointSeq = 0, headSeq = 10)))

        assertEquals(0, checkpointRequests)
        assertFalse(sync.checkpointPending.value)
        assertTrue(doc.imported.isEmpty())
        val rowsReq = ws.sent.last { Chat2Codec.decode(it)?.kind == Chat2Codec.ROWS_REQ }
        assertEquals(0L, Chat2Codec.decode(rowsReq)?.header?.optLong("after"))
    }

    @Test
    fun failedCheckpointKeepsFlagAndStillRequestsRows() = runTest {
        val ws = RecordingWs()
        val doc = RecordingDoc()
        val http = FakeHttpTransport { _, _, _ -> HttpResponse(404, ByteArray(0)) }
        val sync = newSync(ws, doc, http, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        sync.start(
            cursor = 0,
            deviceId = "dev",
            url = "wss://edge.test/chat2/chat-1/ws",
            checkpointUrl = "https://edge.test/chat2/chat-1/checkpoint?token=t",
        )

        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Binary(state(checkpointSize = 1000, checkpointSeq = 5, headSeq = 10)))

        assertTrue("checkpoint missing → 'older messages' flag stays up", sync.checkpointPending.value)
        assertTrue(doc.imported.isEmpty())
        assertTrue(sync.lastError.value?.contains("checkpoint fetch failed") == true)
        // The live path still works: rows from the held cursor.
        val rowsReq = ws.sent.last { Chat2Codec.decode(it)?.kind == Chat2Codec.ROWS_REQ }
        assertEquals(0L, Chat2Codec.decode(rowsReq)?.header?.optLong("after"))
    }

    @Test
    fun sendFlushesPendingPushAfterState() = runTest {
        val ws = RecordingWs()
        val doc = RecordingDoc()
        val http = FakeHttpTransport { _, _, _ -> HttpResponse(200, ByteArray(0)) }
        val sync = newSync(ws, doc, http, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        sync.start(cursor = 0, deviceId = "dev", url = "wss://edge.test/chat2/chat-1/ws")

        // A message queued before the socket handshake completes.
        sync.enqueue("local-update".toByteArray())
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Binary(state(checkpointSize = 0, checkpointSeq = 0, headSeq = 10)))

        val pushes = ws.sent.mapNotNull { Chat2Codec.decode(it) }.filter { it.kind == Chat2Codec.PUSH }
        assertEquals(1, pushes.size)
        assertArrayEquals("local-update".toByteArray(), pushes[0].payload)
        assertNull(sync.lastError.value)
    }
}
