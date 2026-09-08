package sh.zeron.android.sync

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.Dispatchers
import org.json.JSONObject
import sh.zeron.android.data.ChatRow
import sh.zeron.android.data.DocDisk

/**
 * The workspace rows' delivery badges (iOS HomeView sendBadge): for EVERY
 * chat — not just the open one — whether its oldest unadopted send reads
 * Queued or Failed. Derived from the persisted chat2 doc (a send survives
 * relaunch, so the ledger is the durable truth; the open session's live
 * pendingSends override it via [setLive] for a real `started` clock).
 *
 * Derivation mirrors AppModel.sendState: nil = nothing pending; `failed`
 * (unadopted past the 2-minute grace) wins over `queued` (degraded path);
 * healthy in-flight reads `sending`.
 */
class ChatDeliveryTracker(
    private val scope: CoroutineScope,
    private val offline: StateFlow<Boolean>,
    private val presence: StateFlow<Map<String, Long>>,
    private val registryConnected: StateFlow<Boolean>,
    /** Import snapshot bytes into a doc and return its deep-value JSON. */
    private val docReader: suspend (ByteArray) -> String?,
) {
    /** A pending own-command whose user message never landed. */
    data class Pending(val kind: String, val messageId: String, val issuedAt: Long)

    private val _badges = MutableStateFlow<Map<String, SendState?>>(emptyMap())
    val badges: StateFlow<Map<String, SendState?>> = _badges

    private data class Scan(val stamp: Long, val pending: Pending?)

    private var chats: List<ChatRow> = emptyList()
    private val scans = mutableMapOf<String, Scan>()
    /** Live overrides (the open session's real pendingSends). */
    private val live = mutableMapOf<String, SendState?>()
    private var pulseJob: Job? = null

    /** The workspace's chat list — badge keys follow it. */
    fun setChats(chats: List<ChatRow>) {
        val ids = chats.map { it.id }.toSet()
        scans.keys.retainAll(ids)
        live.keys.retainAll(ids)
        this.chats = chats
        scope.launch {
            rescanAll()
            recompute()
        }
    }

    /** The open session's live state (real started clock + live room truth). */
    fun setLive(chatId: String, state: SendState?) {
        if (live[chatId] == state) return
        live[chatId] = state
        recompute()
    }

    /** Event-driven refresh (presence/offline/connected changed). */
    fun recompute() {
        val now = System.currentTimeMillis()
        _badges.value = chats.associate { chat ->
            chat.id to (live[chat.id] ?: derive(chat, scans[chat.id]?.pending, now))
        }
        ensurePulse()
    }

    /** Re-scan every chat whose snapshot stamp changed (new sends landed). */
    suspend fun rescanAll() {
        for (chat in chats) {
            val stamp = DocDisk.snapshotStamp(chat.id)
            if (stamp == null) {
                scans.remove(chat.id)
                continue
            }
            val cached = scans[chat.id]
            if (cached != null && cached.stamp == stamp) continue
            // Cache the null result too — otherwise a no-pending chat would
            // re-scan on every pulse (stamp never changes).
            scans[chat.id] = Scan(stamp, scanPending(chat.id, stamp))
        }
    }

    /**
     * One chat's pending scan: import its snapshot, walk the command ledger
     * for own pending commands whose user message never landed, keep the
     * oldest issuedAt. Null = nothing pending (or unreadable snapshot).
     */
    private suspend fun scanPending(chatId: String, stamp: Long): Pending? =
        withContext(Dispatchers.IO) {
            val bytes = DocDisk.readChat2Snapshot(chatId) ?: return@withContext null
            val json = docReader(bytes) ?: return@withContext null
            parsePending(json)
        }

    private fun parsePending(json: String): Pending? {
        val root = runCatching { JSONObject(json) }.getOrNull() ?: return null
        val landed = runCatching {
            val messages = root.optJSONArray("messages") ?: org.json.JSONArray()
            (0 until messages.length()).mapNotNull { messages.optJSONObject(it)?.optString("id").takeIf { s -> !s.isNullOrEmpty() } }
        }.getOrDefault(emptyList()).toSet()
        val commands = root.optJSONArray("commands") ?: return null
        var oldest: Pending? = null
        for (i in 0 until commands.length()) {
            val m = commands.optJSONObject(i) ?: continue
            if (m.optString("issuedBy") != "android") continue
            val kind = m.optString("kind")
            if (kind != "run" && kind != "steer") continue
            if (m.optString("status", "pending") != "pending") continue
            val payload = m.optJSONObject("payload") ?: continue
            val messageId = payload.optString("messageId")
            if (messageId.isEmpty() || messageId in landed) continue
            if (m.optLong("expiresAt", 0) in 1..System.currentTimeMillis()) continue
            val issuedAt = m.optLong("issuedAt", 0)
            if (oldest == null || issuedAt < oldest.issuedAt) {
                oldest = Pending(kind, messageId, issuedAt)
            }
        }
        return oldest
    }

    /** state.rs derivation (same shape as SessionRepository.sendState). */
    private fun derive(chat: ChatRow, pending: Pending?, now: Long): SendState? {
        if (pending == null) return null
        if (now - pending.issuedAt > UNDELIVERED_GRACE_MS) return SendState.Failed
        if (offline.value || !registryConnected.value || hostDark(chat.deviceId)) {
            return SendState.Queued
        }
        return SendState.Sending
    }

    private fun hostDark(deviceId: String?): Boolean {
        if (deviceId == null) return false
        val seen = presence.value[deviceId] ?: return true
        return System.currentTimeMillis() - seen > 30_000L
    }

    /** 1Hz pulse while any badge is live — flips Sending → Failed at grace. */
    private fun ensurePulse() {
        if (pulseJob != null) return
        if (_badges.value.values.none { it != null }) return
        pulseJob = scope.launch {
            while (isActive) {
                delay(1_000)
                rescanAll()
                recompute()
                if (_badges.value.values.none { it != null }) {
                    pulseJob = null
                    break
                }
            }
        }
    }
}
