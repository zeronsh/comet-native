package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import sh.zeron.android.data.InputAnswer
import sh.zeron.android.data.SessionAdapter
import sh.zeron.android.data.Transcript
import sh.zeron.android.loro.LoroDoc
import java.util.UUID

/**
 * An optimistic echo the host hasn't materialized yet (iOS PendingSend).
 * [started] is the delivery-grace clock, reset by the retry affordance so the
 * surface returns to Sending/Queued.
 */
data class PendingSend(
    val messageId: String,
    val text: String,
    var started: Long,
)

/**
 * One open session: the native Loro doc, its chat2 room, and the transcript
 * projection the UI reads. Writer discipline (docs/chat2-sync.md): the viewer
 * appends command-ledger entries only; the host writes every transcript entry.
 */
class SessionRepository(
    private val chatId: String,
    private val doc: LoroDoc,
    private val adapter: SessionAdapter,
    private val sync: ChatSync,
    private val scope: CoroutineScope,
) {
    private val _transcript = MutableStateFlow(Transcript(emptyList()))
    val transcript: StateFlow<Transcript> = _transcript
    private val _pendingSends = MutableStateFlow<List<PendingSend>>(emptyList())
    val pendingSends: StateFlow<List<PendingSend>> = _pendingSends
    /** Fraction of the current attachment escort's bytes committed (iOS transferProgress). */
    private val _transferProgress = MutableStateFlow<Double?>(null)
    val transferProgress: StateFlow<Double?> = _transferProgress

    val connected: StateFlow<Boolean> = sync.connected
    val lastError: StateFlow<String?> = sync.lastError
    val checkpointPending: StateFlow<Boolean> = sync.checkpointPending

    /** Re-project after every batch of imported rows. */
    fun observe() {
        scope.launch {
            sync.revision.collect { t ->
                val transcript = adapter.transcript()
                _transcript.value = transcript
                // Drop echoes the host has materialized (iOS apply()).
                val landed = transcript.messages.map { it.id }.toSet()
                _pendingSends.value = _pendingSends.value.filter { it.messageId !in landed }
            }
        }
    }

    fun start(cursor: Long, deviceId: String, url: String, checkpointUrl: String? = null) {
        observe()
        sync.start(cursor, deviceId, url, checkpointUrl)
    }

    suspend fun refresh() { _transcript.value = adapter.transcript() }

    /** Append a command, then push the doc update so the host can drain it. */
    private suspend fun queueAndPush(kind: String, payload: JSONObject) {
        adapter.queueCommand(kind, payload.toString())
        sync.enqueue(doc.exportSnapshot())
        _transcript.value = adapter.transcript()
    }

    /**
     * Command payloads MUST match the host schema (crates/doc/src/commands.rs
     * `SessionCommandPayload`, serde-tagged by `kind`). The old Android shape
     * (`{"text": ...}`) failed to deserialize as `Run { request, messageId }`
     * and every entry was silently dropped by `read_commands()` — the
     * send-never-works bug. Each payload carries the harness/model picked on
     * the composer, which the host records as the chat's run config.
     */
    /**
     * [cwd] overrides the run folder (a new session's space/worktree path —
     * default "~" keeps existing chats project-less); [worktree] is the
     * WorktreeSpec the HOST materializes at drain time (iOS NewSessionView);
     * [reasoning] is the picked effort ladder level (null = harness default);
     * [attachments] are the attachment refs (absolute host paths or pending://
     * refs) that RunRequest.attachments carries.
     */
    suspend fun sendPrompt(text: String, harness: String?, model: String?, reasoning: String? = null, cwd: String? = null, worktree: JSONObject? = null, attachments: List<String> = emptyList()) {
        val messageId = UUID.randomUUID().toString().lowercase()
        queueAndPush("run", runPayload(text, harness, model, reasoning, cwd, worktree, messageId, attachments))
        trackPending(messageId, text)
    }
    suspend fun steer(text: String) {
        val messageId = UUID.randomUUID().toString().lowercase()
        queueAndPush("steer", steerPayload(text, messageId))
        trackPending(messageId, text)
    }
    suspend fun interrupt() = queueAndPush("interrupt", JSONObject().put("kind", "interrupt"))
    suspend fun respondInput(requestId: String, answers: List<InputAnswer>) =
        queueAndPush("respondInput", respondInputPayload(requestId, answers))

    private fun runPayload(text: String, harness: String?, model: String?, reasoning: String?, cwd: String?, worktree: JSONObject?, messageId: String, attachments: List<String> = emptyList()): JSONObject {
        // RunRequest (crates/proto/src/agent.rs). "~" is the project-less
        // convention: the creating device can't know the host's home, and the
        // host expands it where the run spawns (sessions.rs expand_home). A new
        // session's first run carries the space/worktree path instead.
        val request = JSONObject()
            .put("prompt", text)
            .put("harness", harness ?: JSONObject.NULL)
            .put("model", model ?: JSONObject.NULL)
            .put("reasoning", reasoning ?: JSONObject.NULL)
            .put("modelOptions", JSONObject())
            .put("cwd", cwd ?: "~")
            .put("sandbox", "workspace-write")
            .put("autoApprove", false)
            .put("resume", JSONObject.NULL)
            .put("attachments", JSONArray().apply { attachments.forEach { put(it) } })
            .put("worktree", worktree ?: JSONObject.NULL)
        return JSONObject()
            .put("kind", "run")
            .put("request", request)
            // Client-minted user-message id: the host writes the user entry
            // under it (dedupe key for re-executed commands / optimistic echo).
            .put("messageId", messageId)
    }

    /** Escort progress ("Uploading… N%") — set by the AppViewModel escort. */
    fun setTransferProgress(fraction: Double?) {
        _transferProgress.value = fraction
    }

    private fun steerPayload(text: String, messageId: String): JSONObject =
        JSONObject()
            .put("kind", "steer")
            .put("prompt", text)
            .put("messageId", messageId)

    private fun trackPending(messageId: String, text: String) {
        val now = System.currentTimeMillis()
        _pendingSends.value = _pendingSends.value + PendingSend(messageId, text, started = now)
    }

    /**
     * state.rs send_* derivation: nil = nothing pending. `failed` (unadopted
     * past the 2-minute grace) wins over `queued` (pending on a degraded
     * path); a healthy in-flight send reads `sending`.
     */
    fun sendState(now: Long, offline: Boolean, hostOnline: Boolean): SendState? {
        val oldest = _pendingSends.value.minOfOrNull { it.started } ?: return null
        if (now - oldest > UNDELIVERED_GRACE_MS) return SendState.Failed
        if (offline || !sync.connected.value || !hostOnline) return SendState.Queued
        return SendState.Sending
    }

    /**
     * The "Not delivered — tap to retry" affordance (iOS retryDelivery):
     * restart the grace clock, re-issue dead attempts (rejected/expired
     * commands for a user message that never landed), and re-push every
     * unacked batch on a fresh kick.
     */
    suspend fun retryDelivery() {
        val now = System.currentTimeMillis()
        _pendingSends.value = _pendingSends.value.map { it.copy(started = now) }
        reissueDeadSends(now)
        sync.kick()
        sync.flushPending()
    }

    /**
     * PR #172's retry semantics, phone half: a Run/Steer whose user message
     * never landed and whose command can never execute again — Rejected,
     * Expired, or Pending past its own TTL — gets a FRESH attempt (new id,
     * same payload and messageId; the host's user-entry pre-write dedupes by
     * message id). One re-issue per message; a live pending attempt skips it.
     */
    private suspend fun reissueDeadSends(now: Long) {
        val landed = _transcript.value.messages.map { it.id }.toSet()
        val json = runCatching { doc.getDeepValueJson() }.getOrNull() ?: return
        val root = runCatching { JSONObject(json) }.getOrNull() ?: return
        val commands = root.optJSONArray("commands") ?: return
        data class DeadAttempt(val kind: String, val payload: JSONObject, val issuedAt: Long)
        val latestDead = mutableMapOf<String, DeadAttempt>()
        val liveMessageIds = mutableSetOf<String>()
        for (i in 0 until commands.length()) {
            val m = commands.optJSONObject(i) ?: continue
            if (m.optString("issuedBy") != "android") continue
            val kind = m.optString("kind")
            if (kind != "run" && kind != "steer") continue
            val payload = m.optJSONObject("payload") ?: continue
            val messageId = payload.optString("messageId")
            if (messageId.isEmpty() || messageId in landed) continue
            val status = m.optString("status", "pending")
            val expired = m.optLong("expiresAt", 0) in 1..now
            if (status == "pending" && !expired) {
                liveMessageIds += messageId
                continue
            }
            if (status != "rejected" && status != "expired" && !(status == "pending" && expired)) continue
            val issuedAt = m.optLong("issuedAt", 0)
            val prev = latestDead[messageId]
            if (prev == null || prev.issuedAt < issuedAt) {
                latestDead[messageId] = DeadAttempt(kind, payload, issuedAt)
            }
        }
        for ((messageId, attempt) in latestDead) {
            if (messageId in liveMessageIds) continue
            val payload = attempt.payload
            // Same payload/messageId, fresh command id — queueCommand mints it.
            adapter.queueCommand(attempt.kind, payload.toString())
            sync.enqueue(doc.exportSnapshot())
        }
    }

    /**
     * session_command_payload respondInput (crates/doc/src/commands.rs): one
     * answer per question, `answers: [{questionId, labels}]` — labels from the
     * picked options, or a single typed custom answer. The part's own id IS
     * the harness-minted request id (schema.rs input part).
     */
    private fun respondInputPayload(requestId: String, answers: List<InputAnswer>): JSONObject {
        val answerArr = JSONArray()
        answers.forEach { a ->
            answerArr.put(JSONObject()
                .put("questionId", a.questionId)
                .put("labels", JSONArray().apply { a.labels.forEach { put(it) } }))
        }
        return JSONObject()
            .put("kind", "respondInput")
            .put("requestId", requestId)
            .put("answers", answerArr)
    }

    /** Network recovery: re-dial the chat2 room (unacked pushes re-send). */
    fun kick() = sync.kick()

    /** Backgrounding hook: persist the snapshot immediately. */
    suspend fun flushToDisk() = sync.flushToDisk()

    suspend fun shutdown() {
        sync.stop()
        doc.closeDoc()
    }
}
