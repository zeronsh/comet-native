package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import sh.zeron.android.data.ChatRow
import sh.zeron.android.data.DeviceRow
import sh.zeron.android.data.HlcClock
import sh.zeron.android.data.RegistryAdapter
import sh.zeron.android.data.RegistryDoc
import sh.zeron.android.data.RegistryRow
import sh.zeron.android.data.SpaceRow
import sh.zeron.android.protocol.RegistryCodec
import sh.zeron.android.protocol.RegistryFrame
import java.util.UUID

/**
 * Registry room client (docs/registry-sync.md). `hello` must be the first frame
 * and can only be sent once the socket is OPEN — the reply is the `state` frame
 * that carries the row snapshot.
 *
 * Presence beats every 15s announce this device to peers (the registry's
 * ephemeral presence map). Like iOS, a phone is a viewport: it publishes
 * presence but owns no `devices` row.
 */
class RegistrySync(
    private val ws: WebSocketTransport,
    private val http: HttpTransport,
    private val scope: CoroutineScope = CoroutineScope(SupervisorJob()),
) {
    private companion object {
        const val PRESENCE_INTERVAL_MS = 15_000L
        /** A device is online while its last presence beat is this fresh (iOS presenceTtlMs). */
        const val PRESENCE_TTL_MS = 30_000L
    }

    private val doc = RegistryDoc()
    private val adapter = RegistryAdapter(doc)
    /** HLC for local LWW writes (iOS RegistryCore.clock). */
    private val clock = HlcClock()

    private val _connected = MutableStateFlow(false)
    val connected: StateFlow<Boolean> = _connected
    private val _chats = MutableStateFlow(adapter.chats())
    val chats: StateFlow<List<ChatRow>> = _chats
    private val _spaces = MutableStateFlow(adapter.spaces())
    val spaces: StateFlow<List<SpaceRow>> = _spaces
    private val _devices = MutableStateFlow(adapter.devices())
    val devices: StateFlow<List<DeviceRow>> = _devices
    /** deviceId → last presence beat ms (iOS WorkspaceStore.presence). */
    private val _presence = MutableStateFlow<Map<String, Long>>(emptyMap())
    val presence: StateFlow<Map<String, Long>> = _presence
    /** Last transport/protocol error, for the UI to show instead of hanging. */
    private val _lastError = MutableStateFlow<String?>(null)
    val lastError: StateFlow<String?> = _lastError

    private var collectJob: Job? = null
    private var presenceJob: Job? = null
    private var cursor: Long? = null
    private var deviceId: String = ""
    private var url: String = ""

    fun start(cursor: Long?, deviceId: String, url: String) {
        stop()
        this.cursor = cursor
        this.deviceId = deviceId
        this.url = url
        _lastError.value = null
        collectJob = ws.connect(url)
            .onEach { msg ->
                when (msg) {
                    // hello goes out only after the socket is open — sending it
                    // before connect() left the frame on a null socket and the
                    // room never answered (the endless "Connecting…" state).
                    is WsMessage.Connected -> {
                        ws.send(WsMessage.Text(RegistryCodec.encode(RegistryFrame.Hello(cursor, deviceId))))
                        startPresence()
                    }
                    is WsMessage.Text -> handleText(msg.text)
                    is WsMessage.Binary -> {}
                    is WsMessage.Closed -> {
                        _connected.value = false
                        doc.markDisconnected() // unacked writes re-push on the next hello
                        presenceJob?.cancel()
                    }
                }
            }
            .catch { e ->
                _connected.value = false
                _lastError.value = e.message ?: "connection failed"
            }
            .launchIn(scope)
    }

    private fun startPresence() {
        presenceJob?.cancel()
        presenceJob = scope.launch {
            while (isActive) {
                ws.send(WsMessage.Text(RegistryCodec.encode(RegistryFrame.Presence(System.currentTimeMillis()))))
                delay(PRESENCE_INTERVAL_MS)
            }
        }
    }

    private suspend fun handleText(text: String) {
        if (text == "pong") return
        val frame = RegistryCodec.decode(text)
        if (frame == null) {
            _lastError.value = "unparseable frame from registry"
            return
        }
        when (frame) {
            is RegistryFrame.State -> {
                doc.applyState(frame.full, parseRows(frame.rows), frame.seq)
                cursor = frame.seq
                if (frame.presence.isNotEmpty()) _presence.value = _presence.value + frame.presence
                _connected.value = true
                publish()
                flushPending()
            }
            is RegistryFrame.PresenceBeat -> {
                _presence.value = _presence.value + (frame.device to frame.at)
            }
            is RegistryFrame.Rows -> {
                doc.applyState(full = false, parseRows(frame.rows), frame.seq)
                cursor = frame.seq
                publish()
            }
            is RegistryFrame.Ack -> {
                doc.retire(frame.batch)
                _lastError.value = null
            }
            is RegistryFrame.Error -> _lastError.value = "${frame.code}: ${frame.message}"
            else -> {}
        }
    }

    private fun parseRows(rowsJson: String): List<RegistryRow> = try {
        val arr = JSONArray(rowsJson)
        (0 until arr.length()).mapNotNull {
            runCatching { RegistryRow.parse(arr.getJSONObject(it)) }.getOrNull()
        }
    } catch (_: Exception) { emptyList() }

    private fun publish() {
        _chats.value = adapter.chats()
        _spaces.value = adapter.spaces()
        _devices.value = adapter.devices()
    }

    /**
     * iOS `setChatConfig`: rewrite the chat row's `config` field (LWW). The
     * whole object is replaced per-field by HLC — the caller merges the picked
     * harness/model into the existing config so reasoning/modelOptions the
     * desktop pickers set are preserved. No row, no write (never invent rows).
     */
    fun setChatConfig(chatId: String, config: JSONObject) {
        if (doc.overlayRow("chats", chatId) == null) return
        val hlc = clock.next(System.currentTimeMillis(), deviceId)
        doc.write("chats", chatId, "update", JSONObject().put("config", config), hlc)
        publish() // the overlay reflects the write immediately
        scope.launch { flushPending() }
    }

    /**
     * iOS WorkspaceStore.createChat: mint a chat row (full-row upsert) bound
     * to the space's owning device — the host picks it up via the registry
     * (claim-on-first-command) and runs the queued first prompt there. Returns
     * the new chat id, or null when the space names no host device.
     *
     * [branch] is the base ref the session is pinned to (chat row `branch`);
     * [cwd] overrides the run folder (a reused worktree path) — defaults to
     * the space folder.
     */
    fun createChat(space: SpaceRow, config: JSONObject, branch: String? = null, cwd: String? = null): String? {
        val hostDevice = space.deviceId ?: return null
        val now = System.currentTimeMillis()
        val chatId = UUID.randomUUID().toString().lowercase()
        val set = JSONObject()
            .put("id", chatId)
            .put("deviceId", hostDevice) // the run happens on the host, not this phone
            .put("archived", false)
            .put("cwd", cwd ?: space.path)
            .put("spaceId", space.id)
            .put("createdAt", now)
            .put("roomGen", 2) // born on chat2: empty doc, nothing to seed
            .put("config", config)
        if (branch != null) set.put("branch", branch)
        val hlc = clock.next(now, this.deviceId) // this phone stamps the write
        doc.write("chats", chatId, "upsert", set, hlc)
        publish() // the new chat shows in the list immediately (pending overlay)
        scope.launch { flushPending() }
        return chatId
    }

    /**
     * Fallback create-space (iOS WorkspaceStore.createSpace): a full-row
     * `spaces` upsert from here, used when the owning host is unreachable —
     * creates are legal from any device; the owner stamps git on arrival.
     * Dedupes on (device, path) like the desktop palette.
     */
    fun createSpace(spaceId: String, deviceId: String, path: String, gitDetected: Boolean) {
        if (doc.overlayRows("spaces").any { it.field("deviceId") == deviceId && it.field("path") == path }) {
            return
        }
        val now = System.currentTimeMillis()
        val set = JSONObject()
            .put("id", spaceId)
            .put("deviceId", deviceId)
            .put("path", path)
            .put("gitDetected", gitDetected)
            .put("createdAt", now)
        val hlc = clock.next(now, this.deviceId)
        doc.write("spaces", spaceId, "upsert", set, hlc)
        publish()
        scope.launch { flushPending() }
    }

    /** The chat row's raw `config` object (overlay applied), for merging edits. */
    fun chatConfig(chatId: String): JSONObject? =
        doc.overlayRow("chats", chatId)?.fields?.optJSONObject("config")

    /** Push every unacked batch (server re-apply is idempotent by HLC compare). */
    private suspend fun flushPending() {
        if (!_connected.value) return
        for (batch in doc.takePushable()) {
            val ops = JSONArray()
            batch.ops.forEach { ops.put(it.toJson()) }
            ws.send(WsMessage.Text(RegistryCodec.encode(RegistryFrame.Push(batch.batch, ops))))
        }
    }

    /**
     * Re-dial the room (network recovery, foreground) with the persisted
     * cursor — unacked pending writes re-push on the fresh hello. The old
     * no-op meant a dropped room stayed dropped until app restart.
     */
    fun kick() {
        if (url.isEmpty()) return
        val cursorNow = cursor
        val deviceNow = deviceId
        collectJob?.cancel()
        collectJob = null
        start(cursorNow, deviceNow, url)
    }

    fun stop() {
        presenceJob?.cancel()
        presenceJob = null
        collectJob?.cancel()
        collectJob = null
        doc.markDisconnected() // writes queued offline survive until the next start
        // The scope outlives a single session — cancelling it here killed every
        // later start() (the collector never ran again).
        scope.launch { ws.close() }
        _connected.value = false
    }
}
