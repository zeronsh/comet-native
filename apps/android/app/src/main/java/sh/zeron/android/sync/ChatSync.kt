package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.launch
import sh.zeron.android.data.DocDisk
import sh.zeron.android.data.DocSaver
import sh.zeron.android.loro.LoroDoc
import sh.zeron.android.protocol.Chat2Codec
import java.util.UUID

/**
 * chat2 room client (docs/chat2-sync.md). One socket per chat:
 * `hello{cursor,device}` → `state` header, then a `rowsReq` backfill whose rows
 * are opaque Loro updates imported into the session doc. Local writes are
 * pushed as `push{batchId}` frames and retired on `ack`.
 *
 * Catch-up follows the client-side precision rule (chat_client.rs
 * `plan_catch_up`): when the room's history was compacted (`checkpointSize >
 * 0`) a fresh reader must `GET /checkpoint` first, then request rows after
 * `checkpointSeq`. Importing the checkpoint before any row is what keeps
 * post-compaction rows from parking on missing causal deps — without it the
 * transcript stayed empty and sends appeared dead. The doc persists to disk
 * (DocDisk chat2 snapshot) so restarts render instantly and backfill
 * incrementally; the checkpoint path stays the always-safe branch.
 */
class ChatSync(
    private val chatId: String,
    private val ws: WebSocketTransport,
    private val http: HttpTransport,
    private val doc: LoroDoc,
    private val scope: CoroutineScope = CoroutineScope(SupervisorJob()),
) {
    /** GET /chat2/{chatId}/checkpoint (auth ?token=) — null when unavailable. */
    private var checkpointUrl: String? = null
    private val _connected = MutableStateFlow(false)
    val connected: StateFlow<Boolean> = _connected
    /** Bumped whenever imported rows changed the doc, so the UI re-reads it. */
    private val _revision = MutableStateFlow(0)
    val revision: StateFlow<Int> = _revision
    private val _lastError = MutableStateFlow<String?>(null)
    val lastError: StateFlow<String?> = _lastError
    /** True when the room has a checkpoint this client cannot fetch yet. */
    private val _checkpointPending = MutableStateFlow(false)
    val checkpointPending: StateFlow<Boolean> = _checkpointPending

    private var collectJob: Job? = null
    private var cursor: Long = 0
    private var deviceId: String = ""
    private var url: String = ""
    private var stateReceived = false
    private val pending = LinkedHashMap<String, ByteArray>()
    /** Debounced snapshot persistence (iOS DocSaver) — doc + cursor, one file. */
    private val saver = DocSaver(scope) { DocDisk.saveChat2(doc, chatId, cursor) }

    fun start(cursor: Long, deviceId: String, url: String, checkpointUrl: String? = null) {
        stop()
        this.cursor = cursor
        this.deviceId = deviceId
        this.url = url
        this.checkpointUrl = checkpointUrl
        stateReceived = false
        _lastError.value = null
        _checkpointPending.value = false
        collectJob = ws.connect(url)
            .onEach { msg ->
                when (msg) {
                    is WsMessage.Connected -> ws.send(WsMessage.Binary(Chat2Codec.hello(cursor, deviceId)))
                    is WsMessage.Binary -> handleFrame(msg.bytes)
                    is WsMessage.Text -> {} // "pong" — transport only
                    is WsMessage.Closed -> {
                        _connected.value = false
                        stateReceived = false
                    }
                }
            }
            .catch { e ->
                _connected.value = false
                _lastError.value = e.message ?: "connection failed"
            }
            .launchIn(scope)
    }

    private suspend fun handleFrame(bytes: ByteArray) {
        val frame = Chat2Codec.decode(bytes) ?: run {
            _lastError.value = "unparseable chat frame"
            return
        }
        when (frame.kind) {
            Chat2Codec.STATE -> handleState(frame)
            Chat2Codec.ROW -> handleRow(frame)
            Chat2Codec.ROWS_DONE -> {
                _revision.value += 1
                saver.poke()
            }
            Chat2Codec.ACK -> {
                val batchId = frame.header.optString("batchId")
                val seq = frame.header.optLong("seq", 0)
                pending.remove(batchId)
                if (seq <= cursor + 1) cursor = seq
                saver.poke()
            }
            Chat2Codec.ERROR -> {
                val code = frame.header.optString("code", "unknown")
                val batchId = frame.header.optString("batchId")
                _lastError.value = "$code: ${frame.header.optString("message")}"
                // Permanent verdicts retire the batch; otherwise it would replay
                // on every reconnect forever.
                if (code in setOf("too_large", "empty", "bad_push") && batchId.isNotEmpty()) {
                    pending.remove(batchId)
                }
            }
            else -> {} // presence / probe-ok / future frames
        }
    }

    /**
     * The hello answer: plan the catch-up (chat_client.rs `plan_catch_up`).
     * Runs inline on the collector so a compacted room's checkpoint imports
     * BEFORE any row does — inbound frames during the fetch queue in the flow
     * buffer and apply after, preserving the checkpoint-then-rows order.
     */
    private suspend fun handleState(frame: Chat2Codec.Frame) {
        val checkpointSize = frame.header.optLong("checkpointSize", 0)
        val checkpointSeq = frame.header.optLong("checkpointSeq", 0)
        val headSeq = frame.header.optLong("headSeq", 0)
        stateReceived = true
        _connected.value = true

        // Cursor amnesty: a cursor above the server's head means the room was
        // reset/wiped (treat as fresh); a cursor above the checkpoint seq
        // claims history the local doc never held — clamp and refetch instead
        // of skipping a hole.
        cursor = when {
            cursor > headSeq -> 0
            checkpointSize > 0 && cursor > checkpointSeq -> checkpointSeq
            else -> cursor
        }

        // Client-side precision (chat2_host.rs): the always-safe branch —
        // fetch and full-state merge rather than trusting a possibly-stale
        // local cursor over the checkpoint frontier (never silently skip
        // history). Loro merge is idempotent, so re-importing what the
        // persisted doc already contains is a no-op.
        if (checkpointSize > 0) {
            _checkpointPending.value = true
            if (fetchAndImportCheckpoint(checkpointSeq)) {
                _checkpointPending.value = false
            }
            // On failure the flag stays up ("older messages unavailable") and
            // the doc still gets whatever live rows follow the held cursor.
        }

        // The plan's `after` IS the cursor now — down (amnesty) or up (the
        // checkpoint covered the skipped span). Without the raise the first
        // backfill row would read as a contiguity gap.
        ws.send(WsMessage.Binary(Chat2Codec.rowsReq(cursor, excludeOwn = false)))
        flushPending()
    }

    private fun handleRow(frame: Chat2Codec.Frame) {
        val seq = frame.header.optLong("seq", 0)
        scope.launch {
            try {
                doc.importBytes(frame.payload)
                // Contiguity: the cursor may walk, never jump a gap.
                if (seq <= cursor + 1) cursor = seq
                _revision.value += 1
                saver.poke()
            } catch (e: Throwable) {
                _lastError.value = "row import failed: ${e.message}"
            }
        }
    }

    /**
     * GET /chat2/{chatId}/checkpoint and import the blob into the session
     * doc. Range resume (iOS/Rust) is not implemented here — a single GET
     * is fine for post-strip KB-scale docs. True = imported.
     */
    private suspend fun fetchAndImportCheckpoint(checkpointSeq: Long): Boolean {
        val url = checkpointUrl ?: return false
        return try {
            val resp = http.get(url)
            if (resp.code != 200) {
                _lastError.value = "checkpoint fetch failed (HTTP ${resp.code})"
                false
            } else if (resp.body.isEmpty()) {
                _lastError.value = "checkpoint fetch returned an empty body"
                false
            } else {
                doc.importBytes(resp.body)
                cursor = maxOf(cursor, checkpointSeq)
                _revision.value += 1
                saver.poke()
                true
            }
        } catch (e: Throwable) {
            _lastError.value = "checkpoint fetch failed: ${e.message}"
            false
        }
    }

    /** Queue a local update for push; survives reconnects until acked. */
    fun enqueue(update: ByteArray) {
        if (update.isEmpty()) return
        pending[UUID.randomUUID().toString().lowercase()] = update
        if (stateReceived) scope.launch { flushPending() }
    }

    /**
     * Re-dial the room (retry taps, network recovery, foreground). The pending
     * batch map survives — unacked pushes go out again on the new link.
     */
    fun kick() {
        if (url.isEmpty()) return
        val checkpoint = checkpointUrl
        val cursorNow = cursor
        val deviceNow = deviceId
        collectJob?.cancel()
        collectJob = null
        start(cursorNow, deviceNow, url, checkpoint)
    }

    /** Re-push every unacked batch (public for the retry affordance). */
    suspend fun flushPending() {
        if (!stateReceived) return
        for ((batchId, bytes) in pending.entries.toList()) {
            ws.send(WsMessage.Binary(Chat2Codec.push(batchId, bytes)))
        }
    }

    fun stop() {
        collectJob?.cancel()
        collectJob = null
        scope.launch {
            ws.close()
            saver.flush()
        }
        _connected.value = false
        stateReceived = false
    }

    /** Backgrounding hook: persist immediately (iOS SessionStore.flushToDisk). */
    suspend fun flushToDisk() {
        saver.flush()
    }
}
