package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.runTest
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Relay wire (iOS DeviceRelayClient parity): uleb128 header frames, ndjson
 * ControlRpc unary calls, ok/err routing by id.
 */
class DeviceRelayClientTest {
    private class RelayWs : FakeWebSocketTransport() {
        val sent = mutableListOf<ByteArray>()
        override suspend fun send(message: WsMessage) {
            if (message is WsMessage.Binary) sent += message.bytes
        }
    }

    private fun newRelay(ws: RelayWs, scope: CoroutineScope) =
        DeviceRelayClient("d1", "wss://edge.test/device/d1/ws?role=client", ws, scope)

    private fun rpcFrame(ws: RelayWs): DeviceRelayClient.Frame =
        ws.sent.mapNotNull { DeviceRelayClient.decodeFrame(it) }
            .first { String(it.payload).contains("\"method\"") }

    @Test
    fun codecRoundTripsHeaderAndPayload() {
        val frame = DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            "{\"id\":1,\"method\":\"ListRefs\"}".toByteArray(),
        )
        val decoded = DeviceRelayClient.decodeFrame(frame)
        assertEquals("rpc", decoded?.header?.getString("k"))
        assertEquals("{\"id\":1,\"method\":\"ListRefs\"}", String(decoded!!.payload))
    }

    @Test
    fun callRoutesOkPayloadById() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.call("ListRefs", JSONObject().put("repoPath", "/home/u/proj")) }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val req = rpcFrame(ws)
        assertEquals("rpc", req.header.getString("k"))
        val id = JSONObject(String(req.payload)).getLong("id")
        assertEquals("ListRefs", JSONObject(String(req.payload)).getString("method"))

        val reply = DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"ok":[{"name":"main","current":true}]}""".toByteArray(),
        )
        ws.emit(WsMessage.Binary(reply))

        val ok = result.await().getOrThrow()
        val arr = JSONArray(ok)
        assertEquals(1, arr.length())
        assertEquals("main", arr.getJSONObject(0).getString("name"))
    }

    @Test
    fun listRefsParsesRepoRefs() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.listRefs("/home/u/proj") }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val id = JSONObject(String(rpcFrame(ws).payload)).getLong("id")
        ws.emit(WsMessage.Binary(DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"ok":[{"name":"feature/x","current":false,"worktreePath":"/w/x"},
                                {"name":"main","current":true,"worktreePath":null}]}""".toByteArray(),
        )))

        val refs = result.await().getOrThrow()
        assertEquals(2, refs.size)
        assertEquals("feature/x", refs[0].name)
        assertEquals("/w/x", refs[0].worktreePath)
        assertTrue(refs[1].current)
    }

    @Test
    fun listHarnessesFiltersToInstalledAndEnabled() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.listHarnesses() }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val id = JSONObject(String(rpcFrame(ws).payload)).getLong("id")
        ws.emit(WsMessage.Binary(DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"ok":[
                {"id":"claude-code","name":"Claude Code","installed":true,"enabled":true},
                {"id":"codex","name":"Codex","installed":true},
                {"id":"grok","name":"Grok","installed":false,"enabled":true},
                {"id":"mock","name":"Mock","installed":true},
                {"id":"pi","name":"Pi","installed":true,"enabled":false}
            ]}""".toByteArray(),
        )))

        val harnesses = result.await().getOrThrow()
        assertEquals(listOf("claude-code", "codex"), harnesses.map { it.id })
    }

    @Test
    fun listFoldersParsesListing() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.listFolders("/home/u") }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val id = JSONObject(String(rpcFrame(ws).payload)).getLong("id")
        ws.emit(WsMessage.Binary(DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"ok":{"path":"/home/u","truncated":true,"entries":[
                {"name":"proj","isDir":true,"isRepo":true},
                {"name":"docs","isDir":true,"isRepo":false},
                {"name":"file.txt","isDir":false,"isRepo":false}
            ]}}""".toByteArray(),
        )))

        val listing = result.await().getOrThrow()
        assertEquals("/home/u", listing.path)
        assertEquals("/home", listing.parent)
        assertTrue(listing.truncated)
        assertEquals(2, listing.entries.count { it.isDir })
        assertTrue(listing.entries.first { it.name == "proj" }.isRepo)
    }

    @Test
    fun listModelsParsesReasoningLevels() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.listModels("codex") }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val id = JSONObject(String(rpcFrame(ws).payload)).getLong("id")
        ws.emit(WsMessage.Binary(DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"ok":[{"id":"m1","label":"M1","description":"d","reasoningLevels":["low","high"]}]}""".toByteArray(),
        )))

        val models = result.await().getOrThrow()
        assertEquals("m1", models[0].id)
        assertEquals(listOf("low", "high"), models[0].reasoningLevels)
    }

    @Test
    fun errReplyFailsTheCall() = runTest {
        val ws = RelayWs()
        val scope = CoroutineScope(UnconfinedTestDispatcher(testScheduler) + SupervisorJob())
        val relay = newRelay(ws, scope)
        val result = async { relay.switchRef("/p", "main") }

        ws.emit(WsMessage.Connected)
        advanceUntilIdle()
        val id = JSONObject(String(rpcFrame(ws).payload)).getLong("id")
        ws.emit(WsMessage.Binary(DeviceRelayClient.encodeFrame(
            "{\"s\":\"rpc\",\"k\":\"rpc\"}",
            """{"id":$id,"err":"dirty tree"}""".toByteArray(),
        )))

        val failure = result.await()
        assertTrue(failure.isFailure)
        assertEquals("dirty tree", failure.exceptionOrNull()?.message)
    }
}
