package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import sh.zeron.android.data.FolderEntry
import sh.zeron.android.data.FolderListing
import sh.zeron.android.data.HarnessInfo
import sh.zeron.android.data.ModelInfo
import sh.zeron.android.data.RepoRef
import java.io.ByteArrayOutputStream

/** A relay RPC failure — message is user-safe ("device offline", "timeout", …). */
class RelayException(message: String) : Exception(message)

/**
 * Device-room relay RPC client (iOS DeviceRelayClient parity) — dials a
 * device's room on the edge as a `client` peer and speaks ControlRpc to the
 * HOST engine over a virtual socket (crates/rpc/src/device_room.rs +
 * edge/src/device-room.ts).
 *
 * Frame codec (binary WS messages): uleb128(headerLen) ‖ headerJSON ‖ payload.
 * Header key order MUST be {"s","k","to","from"} (byte parity with both
 * implementations); clients never set `to`/`from` — the DO stamps `from`.
 * RPC payloads are ndjson ControlRpc frames: {id, method, params} out,
 * {id, ok|err} back. Keepalive rides a text "ping" + an `echo` frame every
 * 10s (iOS DeviceRelayClient.keepaliveTick — lean port: no echo-deadline
 * enforcement, which only matters for long-lived attachment uploads).
 */
class DeviceRelayClient(
    val deviceId: String,
    private val url: String,
    private val ws: WebSocketTransport,
    private val scope: CoroutineScope,
) {
    private val pending = mutableMapOf<Long, CompletableDeferred<String>>()
    private var nextId = 1L
    private var collectJob: Job? = null
    private var pingJob: Job? = null

    @Volatile private var connected = false
    @Volatile private var dead = true

    /** True while the link is up (or dialable) — stale clients get replaced. */
    val isUsable: Boolean get() = collectJob != null && !dead

    /** Dial (idempotent): starts the collector + keepalive; a dead link redials. */
    fun connect() {
        if (collectJob != null && !dead) return
        dead = false
        connected = false
        collectJob?.cancel()
        collectJob = ws.connect(url)
            .onEach { msg ->
                when (msg) {
                    is WsMessage.Connected -> {
                        connected = true
                        sendEcho()
                    }
                    is WsMessage.Binary -> handleBinary(msg.bytes)
                    is WsMessage.Text -> {} // "pong" — transport only
                    is WsMessage.Closed -> {
                        connected = false
                        dead = true
                        failAll("device offline")
                    }
                }
            }
            .catch { e ->
                connected = false
                dead = true
                failAll(e.message ?: "relay failed")
            }
            .launchIn(scope)
        pingJob?.cancel()
        pingJob = scope.launch {
            while (isActive) {
                delay(10_000)
                if (connected) {
                    ws.send(WsMessage.Text("ping"))
                    sendEcho()
                }
            }
        }
    }

    /**
     * One unary ControlRpc call; the ok payload as raw JSON text (or "null").
     * [timeoutSeconds] defaults to 10s for interactive calls; attachment
     * uploads pass longer ones (first chunks 90s for a cold dial, commit 150s
     * to outlast the cross-device assemble). Timeouts become failures; real
     * cancellations propagate untouched.
     */
    suspend fun call(method: String, params: JSONObject, timeoutSeconds: Long = 10): Result<String> {
        val registered: Pair<Long, CompletableDeferred<String>> = try {
            connect()
            withTimeout(10_000) { while (!connected) delay(50) }
            val id = nextId++
            val payload = JSONObject().put("id", id).put("method", method).put("params", params).toString()
            // Register before sending: a fast host reply must not race ahead of
            // its waiter (iOS callOnce's ordering note).
            val deferred = CompletableDeferred<String>()
            pending[id] = deferred
            ws.send(WsMessage.Binary(encodeFrame(RPC_HEADER, payload.toByteArray())))
            id to deferred
        } catch (e: kotlinx.coroutines.TimeoutCancellationException) {
            return Result.failure(RelayException("the device didn't respond"))
        }
        val (id, deferred) = registered
        return try {
            Result.success(withTimeout(timeoutSeconds * 1_000) { deferred.await() })
        } catch (e: kotlinx.coroutines.TimeoutCancellationException) {
            pending.remove(id)
            Result.failure(RelayException("the device didn't respond"))
        } catch (e: RelayException) {
            pending.remove(id)
            Result.failure(e)
        }
    }

    /**
     * ReadAttachmentChunk → one base64 image chunk (name, data, nextOffset,
     * done) — the transcript thumbnail read-back loop. The engine path-jails
     * to the uploads dir + workspace-known chat cwds.
     */
    suspend fun readAttachmentChunk(path: String, offset: Long): Result<AttachmentChunk> =
        call("ReadAttachmentChunk", JSONObject().put("path", path).put("offset", offset), timeoutSeconds = 20)
            .map { ok ->
                val o = JSONObject(ok)
                AttachmentChunk(
                    name = o.optString("name"),
                    data = o.optString("data"),
                    nextOffset = o.optLong("nextOffset", 0),
                    done = o.optBoolean("done", false),
                )
            }

    /** One ReadAttachmentChunk reply. */
    data class AttachmentChunk(val name: String, val data: String, val nextOffset: Long, val done: Boolean)

    /** ListRefs → the host's ref list for the space folder. */
    suspend fun listRefs(repoPath: String): Result<List<RepoRef>> =
        call("ListRefs", JSONObject().put("repoPath", repoPath)).map { ok ->
            val arr = JSONArray(ok)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                RepoRef(
                    name = o.optString("name"),
                    current = o.optBoolean("current", false),
                    worktreePath = o.optString("worktreePath").takeIf { it.isNotEmpty() },
                )
            }
        }

    /** SwitchRef → git checkout in the space folder on the host. */
    suspend fun switchRef(repoPath: String, refName: String): Result<Unit> =
        call("SwitchRef", JSONObject().put("repoPath", repoPath).put("refName", refName)).map { }

    /**
     * ListHarnesses → the host's live harness catalog, filtered to what the
     * composer may offer: installed AND enabled (the desktop Settings → Agents
     * gate; absent `enabled` falls back to the engine's default pair). iOS
     * WorkspaceStore.listHarnesses parity — `mock` never offered.
     */
    suspend fun listHarnesses(): Result<List<HarnessInfo>> =
        call("ListHarnesses", JSONObject()).map { ok ->
            val arr = JSONArray(ok)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                val id = o.optString("id")
                if (id.isEmpty() || id == "mock") return@mapNotNull null
                val installed = o.optBoolean("installed", true)
                val enabled = if (o.has("enabled") && !o.isNull("enabled")) o.optBoolean("enabled")
                else id == "claude-code" || id == "codex"
                if (!installed || !enabled) return@mapNotNull null
                HarnessInfo(id, o.optString("name", id))
            }
        }

    /**
     * ListFolders → the host's folder listing for the add-space browser (nil
     * path = the device's home; the engine caps at 500 entries and hides
     * dotfiles). iOS WorkspaceStore.listFolders parity.
     */
    suspend fun listFolders(path: String?): Result<FolderListing> {
        val params = JSONObject()
        if (path != null) params.put("path", path)
        return call("ListFolders", params).map { ok ->
            val o = JSONObject(ok)
            val entries = o.optJSONArray("entries")?.let { arr ->
                (0 until arr.length()).mapNotNull { i ->
                    val e = arr.optJSONObject(i) ?: return@mapNotNull null
                    FolderEntry(
                        name = e.optString("name"),
                        isDir = e.optBoolean("isDir", false),
                        isRepo = e.optBoolean("isRepo", false),
                    )
                }
            } ?: emptyList()
            FolderListing(
                path = o.optString("path"),
                entries = entries,
                truncated = o.optBoolean("truncated", false),
            )
        }
    }

    /**
     * Mutate {op:createSpace} straight to the owning host — it applies the row
     * to its own registry doc (iOS WorkspaceStore.createSpace preferred path).
     * True when the host accepted; the caller falls back to a local upsert.
     */
    suspend fun mutateCreateSpace(spaceId: String, deviceId: String, path: String, gitDetected: Boolean): Result<Boolean> =
        call("Mutate", JSONObject()
            .put("op", "createSpace")
            .put("spaceId", spaceId)
            .put("deviceId", deviceId)
            .put("path", path)
            .put("gitDetected", gitDetected))
            .map { true }

    /** ListModels → the host's live model catalog for one harness. */
    suspend fun listModels(harness: String): Result<List<ModelInfo>> =
        call("ListModels", JSONObject().put("harness", harness)).map { ok ->
            val arr = JSONArray(ok)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                val id = o.optString("id")
                if (id.isEmpty()) return@mapNotNull null
                ModelInfo(
                    id = id,
                    label = o.optString("label", id),
                    description = o.optString("description").takeIf { it.isNotEmpty() },
                    reasoningLevels = o.optJSONArray("reasoningLevels")?.let { arr ->
                        (0 until arr.length()).mapNotNull { arr.optString(it).takeIf { s -> s.isNotEmpty() } }
                    } ?: emptyList(),
                )
            }
        }

    fun close() {
        collectJob?.cancel()
        collectJob = null
        pingJob?.cancel()
        pingJob = null
        connected = false
        dead = true
        scope.launch { ws.close() }
        failAll("relay closed")
    }

    // MARK: Wire

    private fun handleBinary(bytes: ByteArray) {
        val frame = decodeFrame(bytes) ?: return
        if (frame.header.optString("k") != "rpc") return
        val text = String(frame.payload, Charsets.UTF_8)
        for (line in text.split("\n")) {
            if (line.isBlank()) continue
            val obj = runCatching { JSONObject(line) }.getOrNull() ?: continue
            val id = obj.optLong("id", -1)
            if (id < 0) continue
            val done = pending.remove(id) ?: continue
            when {
                obj.has("err") -> done.completeExceptionally(RelayException(obj.optString("err")))
                obj.has("ok") -> done.complete(obj.opt("ok")?.toString() ?: "null")
                else -> done.completeExceptionally(RelayException("unexpected reply"))
            }
        }
    }

    private fun failAll(error: String) {
        for (deferred in pending.values) deferred.completeExceptionally(RelayException(error))
        pending.clear()
    }

    // Suspend: every call site is already a coroutine (the flow collector's
    // onEach action and the keepalive launch).
    private suspend fun sendEcho() {
        ws.send(WsMessage.Binary(encodeFrame(ECHO_HEADER, ByteArray(0))))
    }

    companion object {
        private const val RPC_HEADER = "{\"s\":\"rpc\",\"k\":\"rpc\"}"
        private const val ECHO_HEADER = "{\"s\":\"echo\",\"k\":\"echo\"}"

        /** uleb128(headerLen) ‖ headerJSON ‖ payload (iOS encodeFrame). */
        fun encodeFrame(header: String, payload: ByteArray): ByteArray {
            val headerBytes = header.toByteArray()
            var len = headerBytes.size
            val out = ByteArrayOutputStream()
            do {
                var byte = (len and 0x7f).toByte()
                len = len ushr 7
                if (len != 0) byte = (byte.toInt() or 0x80).toByte()
                out.write(byte.toInt())
            } while (len != 0)
            out.write(headerBytes)
            out.write(payload)
            return out.toByteArray()
        }

        data class Frame(val header: JSONObject, val payload: ByteArray)

        /** Byte-exact inverse of [encodeFrame] (iOS decodeFrame, shift cap 28). */
        fun decodeFrame(data: ByteArray): Frame? {
            var offset = 0
            var length = 0L
            var shift = 0
            while (offset < data.size) {
                val byte = data[offset].toInt() and 0xff
                offset += 1
                length = length or ((byte and 0x7f).toLong() shl shift)
                if (byte and 0x80 == 0) break
                shift += 7
                if (shift > 28) return null
            }
            if (offset + length > data.size) return null
            val header = runCatching {
                JSONObject(String(data, offset, length.toInt(), Charsets.UTF_8))
            }.getOrNull() ?: return null
            val payload = data.copyOfRange(offset + length.toInt(), data.size)
            return Frame(header, payload)
        }
    }
}
