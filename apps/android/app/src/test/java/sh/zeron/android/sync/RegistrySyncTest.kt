package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import sh.zeron.android.data.SpaceRow

/**
 * Registry write path (iOS setChatConfig parity): a config pick rewrites the
 * chat row's `config` field as an LWW `update` op, pushed in a `push` batch,
 * reflected in the local overlay immediately, and surviving reconnects until
 * the server acks the batch.
 */
class RegistrySyncTest {
    private class RecordingWs : FakeWebSocketTransport() {
        val sentText = mutableListOf<String>()
        override suspend fun send(message: WsMessage) {
            if (message is WsMessage.Text) sentText += message.text
        }
    }

    private fun stateFrame(rows: String) = """{"t":"state","seq":1,"full":true,"gcFloor":0,"rows":$rows}"""

    private fun chatRow(fields: String) = """[{"kind":"chats","id":"c1","seq":1,"deleted":false,
        "fields":$fields,"clocks":{}}]"""

    private val bareChat = """{"id":"c1","deviceId":"d","archived":false}"""

    private fun newSync(ws: RecordingWs, testScope: CoroutineScope): RegistrySync {
        val sync = RegistrySync(ws, FakeHttpTransport(), testScope)
        sync.start(cursor = null, deviceId = "dev", url = "wss://edge.test/registry/ws")
        return sync
    }

    private fun lastPush(sent: List<String>): JSONObject =
        JSONObject(sent.last { JSONObject(it).optString("t") == "push" })

    @Test
    fun setChatConfigPushesUpdateOpAndReflectsImmediately() = runTest {
        val ws = RecordingWs()
        val sync = newSync(ws, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(chatRow(bareChat))))

        sync.setChatConfig("c1", JSONObject().put("harness", "claude-code").put("model", "claude-sonnet-5"))

        val push = lastPush(ws.sentText)
        assertEquals("push", push.optString("t"))
        assertNotNull(push.optString("batch"))
        val ops = push.getJSONArray("ops")
        assertEquals(1, ops.length())
        val op = ops.getJSONObject(0)
        assertEquals("chats", op.getString("kind"))
        assertEquals("c1", op.getString("id"))
        assertEquals("update", op.getString("op"))
        val hlc = op.getString("hlc")
        assertTrue("HLC carries the device suffix", hlc.endsWith("-dev"))
        val config = op.getJSONObject("set").getJSONObject("config")
        assertEquals("claude-code", config.getString("harness"))
        assertEquals("claude-sonnet-5", config.getString("model"))

        // The local overlay shows the write before the server acks it.
        assertEquals("claude-sonnet-5", sync.chats.value.first { it.id == "c1" }.config?.model)
    }

    @Test
    fun offlineConfigWriteQueuesAndFlushesAfterReconnect() = runTest {
        val ws = RecordingWs()
        val sync = newSync(ws, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(chatRow(bareChat))))

        // Drop the connection; the write queues instead of pushing.
        ws.emit(WsMessage.Closed)
        val sentBefore = ws.sentText.size
        sync.setChatConfig("c1", JSONObject().put("harness", "codex").put("model", "gpt-5.4-mini"))
        assertEquals("queued while offline, nothing pushed", sentBefore, ws.sentText.size)
        assertEquals("overlay still reflects the write", "gpt-5.4-mini", sync.chats.value.first { it.id == "c1" }.config?.model)

        // Reconnect: hello → state, then the queued batch goes out.
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(chatRow(bareChat))))
        val op = lastPush(ws.sentText).getJSONArray("ops").getJSONObject(0)
        val config = op.getJSONObject("set").getJSONObject("config")
        assertEquals("codex", config.getString("harness"))
        assertEquals("gpt-5.4-mini", config.getString("model"))
    }

    @Test
    fun createChatUpsertsRowBoundToHostDevice() = runTest {
        val ws = RecordingWs()
        val sync = newSync(ws, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        val spacesRows = """[{"kind":"spaces","id":"s1","seq":1,"deleted":false,
            "fields":{"id":"s1","deviceId":"desktop-1","path":"/home/u/proj"},"clocks":{}}]"""
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(spacesRows)))

        val chatId = sync.createChat(
            SpaceRow(id = "s1", path = "/home/u/proj", deviceId = "desktop-1"),
            JSONObject().put("harness", "claude-code").put("model", "claude-sonnet-5"),
        )
        assertNotNull(chatId)

        val op = lastPush(ws.sentText).getJSONArray("ops").getJSONObject(0)
        assertEquals("upsert", op.getString("op"))
        assertEquals(chatId, op.getString("id"))
        val set = op.getJSONObject("set")
        assertEquals("desktop-1", set.getString("deviceId")) // the host runs it, not the phone
        assertEquals("s1", set.getString("spaceId"))
        assertEquals("/home/u/proj", set.getString("cwd"))
        assertEquals(2, set.getInt("roomGen"))
        assertEquals("claude-code", set.getJSONObject("config").getString("harness"))

        // The overlay shows the brand-new chat immediately (pending upsert).
        val chat = sync.chats.value.first { it.id == chatId }
        assertEquals("s1", chat.spaceId)
        assertEquals("claude-code", chat.config?.harness)
        assertEquals("claude-sonnet-5", chat.config?.model)

        // A space naming no host device cannot mint a chat.
        assertNull(sync.createChat(SpaceRow(id = "s2", path = "/x", deviceId = null), JSONObject()))
    }

    @Test
    fun devicesAndPresenceSurfaceFromState() = runTest {
        val ws = RecordingWs()
        val sync = newSync(ws, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        val state = """{"t":"state","seq":1,"full":true,"gcFloor":0,
            "rows":[{"kind":"devices","id":"desktop-1","seq":1,"deleted":false,
                      "fields":{"id":"desktop-1","name":"MacBook Pro"},"clocks":{}}],
            "presence":{"desktop-1":123456789}}"""
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(state))

        // The device row feeds the space picker's "@ name" tag.
        assertEquals("MacBook Pro", sync.devices.value.first { it.id == "desktop-1" }.name)
        assertEquals(123456789L, sync.presence.value["desktop-1"] ?: 0L)

        // A live presence beat refreshes the map (online staleness window).
        ws.emit(WsMessage.Text("""{"t":"presence","device":"desktop-1","at":987654321}"""))
        assertEquals(987654321L, sync.presence.value["desktop-1"] ?: 0L)
    }

    @Test
    fun ackRetiresTheBatchSoReconnectPushesOnlyNewWrites() = runTest {
        val ws = RecordingWs()
        val sync = newSync(ws, CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob()))
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(chatRow(bareChat))))

        sync.setChatConfig("c1", JSONObject().put("harness", "claude-code").put("model", "claude-sonnet-5"))
        val batch = lastPush(ws.sentText).getString("batch")

        // Server acks the batch; a reconnect must NOT re-push it.
        ws.emit(WsMessage.Text("""{"t":"ack","batch":"$batch","seq":2,"applied":1}"""))
        ws.emit(WsMessage.Closed)
        val before = ws.sentText.size
        ws.emit(WsMessage.Connected)
        ws.emit(WsMessage.Text(stateFrame(chatRow(bareChat))))

        val pushesAfterReconnect = ws.sentText.drop(before).count { JSONObject(it).optString("t") == "push" }
        assertEquals("acked batch retired — no re-push", 0, pushesAfterReconnect)
    }
}
