package sh.zeron.android.ui

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject
import sh.zeron.android.auth.AuthOrg
import sh.zeron.android.auth.AuthStateMachine
import sh.zeron.android.config.AppConfig
import sh.zeron.android.config.EdgeConfig
import sh.zeron.android.config.WorkOsAuth
import sh.zeron.android.core.AppError
import sh.zeron.android.data.AttachmentImageCache
import sh.zeron.android.data.AttachmentTransfer
import sh.zeron.android.data.FolderListing
import sh.zeron.android.data.HarnessCatalog
import sh.zeron.android.data.HarnessInfo
import sh.zeron.android.data.InputAnswer
import sh.zeron.android.data.ModelInfo
import sh.zeron.android.data.Part
import sh.zeron.android.data.RepoRef
import sh.zeron.android.data.SessionAdapter
import sh.zeron.android.data.SpaceRow
import sh.zeron.android.data.StagedAttachment
import sh.zeron.android.data.Transcript
import sh.zeron.android.data.UploadStash
import sh.zeron.android.data.uploadAttachmentChunked
import sh.zeron.android.data.withAttachments
import java.util.UUID
import sh.zeron.android.loro.LoroDoc
import sh.zeron.android.loro.RealNativeLoroDoc
import sh.zeron.android.sync.AppState
import sh.zeron.android.sync.ChatDeliveryTracker
import sh.zeron.android.sync.ChatSync
import sh.zeron.android.sync.Connectivity
import sh.zeron.android.sync.DeviceRelayClient
import sh.zeron.android.sync.HttpTransport
import sh.zeron.android.sync.OkHttpWebSocket
import sh.zeron.android.sync.OnlineBus
import sh.zeron.android.sync.RegistrySync
import sh.zeron.android.sync.SendState
import sh.zeron.android.sync.SessionRepository
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

class AppViewModel(
    private val auth: AuthStateMachine,
    private val registry: RegistrySync,
    private val http: HttpTransport,
    private val config: AppConfig,
) : ViewModel() {
    private val _state = MutableStateFlow<AppState>(AppState.SignedOut)
    val state: StateFlow<AppState> = _state
    val chats = registry.chats

    private val _selectedChat = MutableStateFlow<String?>(null)
    val selectedChat: StateFlow<String?> = _selectedChat
    private val _newSessionOpen = MutableStateFlow(false)
    val newSessionOpen: StateFlow<Boolean> = _newSessionOpen

    /** False when the registry room isn't joined yet (no chats to show). */
    val registryConnected = registry.connected

    /** Surfaced so a failed room join shows a reason instead of a spinner. */
    val registryError = registry.lastError

    private val _transcript = MutableStateFlow(Transcript(emptyList()))
    val transcript: StateFlow<Transcript> = _transcript
    private val _sessionStatus = MutableStateFlow<SessionStatus>(SessionStatus.Connecting)
    val sessionStatus: StateFlow<SessionStatus> = _sessionStatus
    private val _sending = MutableStateFlow(false)
    val sending: StateFlow<Boolean> = _sending
    /** Delivery truth of the open session (iOS AppModel.sendState). */
    private val _sendState = MutableStateFlow<SendState?>(null)
    val sendState: StateFlow<SendState?> = _sendState
    /** Attachment upload progress of the open session ("Uploading… N%"). */
    private val _transferProgress = MutableStateFlow<Double?>(null)
    val transferProgress: StateFlow<Double?> = _transferProgress
    private var sendStateJob: Job? = null

    /** Harness/model the composer will run the next prompt with. */
    private val _modelSelection = MutableStateFlow(HarnessCatalog.defaultSelection())
    val modelSelection: StateFlow<HarnessCatalog.Selection> = _modelSelection

    /**
     * True once the session's own config is known: iOS-parity `lockedHarness`
     * — the provider is set-once on the host (stamped on the first run, never
     * updated), so the harness is locked mid-session while the model stays
     * switchable within it. Only a config-less chat lets the picker choose
     * the provider too.
     */
    private val _harnessLocked = MutableStateFlow(false)
    val harnessLocked: StateFlow<Boolean> = _harnessLocked

    /**
     * Live per-harness model catalogs for the OPEN session (iOS ComposerView
     * `.task(id: chat.id/harness)`): the host's own ListModels answer over the
     * relay, static fallback when the host is dark.
     */
    private val _sessionCatalogs = MutableStateFlow<Map<String, List<ModelInfo>>>(emptyMap())
    val sessionCatalogs: StateFlow<Map<String, List<ModelInfo>>> = _sessionCatalogs

    /**
     * The unresolved agent question to surface instead of the composer (iOS
     * SessionStore.openInputRequest) — nil when nothing is waiting on input.
     */
    private val _openInputRequest = MutableStateFlow<Part.Input?>(null)
    val openInputRequest: StateFlow<Part.Input?> = _openInputRequest

    /** OS network path offline (iOS ConnectivityCenter.state == .offline). */
    val offline = Connectivity.offline

    /**
     * Pre-send honesty (iOS chatDeliveryDegraded): a prompt sent right now
     * would queue rather than deliver — the OS path is down, the chat2 room
     * is down, or the host device's presence is dark. The composer caption
     * owns the copy.
     */
    private val _deliveryDegraded = MutableStateFlow(false)
    val deliveryDegraded: StateFlow<Boolean> = _deliveryDegraded

    /** Spaces to create sessions in (each is a folder on a desktop device). */
    val spaces = registry.spaces

    /** Host devices (name for the space picker, iOS WorkspaceStore.devices). */
    val devices = registry.devices

    /** deviceId → last presence beat ms (online state for the space picker). */
    val presence = registry.presence

    /** Git refs of the selected new-session space, via relay RPC to its host. */
    private val _newSessionRefs = MutableStateFlow<List<RepoRef>?>(null)
    val newSessionRefs: StateFlow<List<RepoRef>?>
        get() = _newSessionRefs
    private val _newSessionRefsLoading = MutableStateFlow(false)
    val newSessionRefsLoading: StateFlow<Boolean>
        get() = _newSessionRefsLoading

    /**
     * The selected space's host's live harness list (iOS NewSessionView
     * `liveHarnesses`): static `default_enabled()` pair until it loads — a
     * reachable host answers with its Settings → Agents catalog.
     */
    private val _newSessionHarnesses = MutableStateFlow<List<HarnessInfo>?>(null)
    val newSessionHarnesses: StateFlow<List<HarnessInfo>?>
        get() = _newSessionHarnesses
    /** Live per-harness model catalogs from the host (static fallback). */
    private val _newSessionCatalogs = MutableStateFlow<Map<String, List<ModelInfo>>>(emptyMap())
    val newSessionCatalogs: StateFlow<Map<String, List<ModelInfo>>>
        get() = _newSessionCatalogs

    /** Relay to the new-session space's host (one per open new-session flow). */
    private var relay: DeviceRelayClient? = null

    /** Relay for the add-space folder browser (separate from the new-session one). */
    private var spaceRelay: DeviceRelayClient? = null

    private var openDoc: LoroDoc? = null
    private var session: SessionRepository? = null
    private var sessionChatId: String? = null
    private var orgId: String? = null

    /** Presence TTL — a host is "online" while its beat is this fresh (iOS). */
    private val presenceTtlMs = 30_000L

    /** Queued-attachment version gate (composer.rs QUEUED_ATTACHMENTS_MIN). */
    private val queuedAttachmentsMin = Triple(0, 2, 12)
    private val transferBackoffBaseMs = 2_000L
    private val transferBackoffCapMs = 30_000L
    private val attachmentWaitMaxMs = 15 * 60_000L
    private val attachmentLoadMutex = Mutex()

    /** CSRF state for the in-flight AuthKit round trip. */
    private var pendingState: String? = null

    /**
     * Workspace delivery badges for EVERY chat (iOS HomeView sendBadge), from
     * each chat's persisted doc + live connectivity — the open session's real
     * pendingSends override it via setLive.
     */
    private val deliveryTracker = ChatDeliveryTracker(
        scope = viewModelScope,
        offline = Connectivity.offline,
        presence = registry.presence,
        registryConnected = registry.connected,
    ) { bytes ->
        val doc = try { RealNativeLoroDoc() } catch (e: Throwable) { return@ChatDeliveryTracker null }
        try {
            doc.importBytes(bytes)
            doc.getDeepValueJson()
        } catch (e: Throwable) {
            null
        } finally {
            doc.closeDoc()
        }
    }
    val deliveryBadges: StateFlow<Map<String, SendState?>> = deliveryTracker.badges

    init {
        UploadStash.sweep() // drop expired pending:// bytes (24h TTL)
        // iOS AppModel.restore parity: resume the persisted session instead of
        // always landing on the sign-in screen (tokens live in SecureTokenStore).
        viewModelScope.launch { restoreSession() }
        viewModelScope.launch {
            registry.chats.collect { deliveryTracker.setChats(it) }
        }
        viewModelScope.launch {
            // Degradation inputs: OS path, host presence, room state.
            Connectivity.offline.collect { deliveryTracker.recompute(); recomputeDeliveryDegraded() }
            registry.presence.collect { deliveryTracker.recompute(); recomputeDeliveryDegraded() }
            registry.connected.collect { deliveryTracker.recompute(); recomputeDeliveryDegraded() }
        }
    }

    /**
     * Restore: load the persisted token pair + org, refresh the org-scoped
     * pair (fails fast when the refresh token is revoked/expired), then join
     * the registry room. A dead token bounces to sign-in; an offline launch
     * keeps the stored pair and lets the socket gate decide. Sessions stored
     * before orgId was persisted re-derive it (single org auto-connects, like
     * iOS; multi-org lands on the picker). Any failure falls back to login.
     */
    private suspend fun restoreSession() {
        try {
            var org = auth.restoreSession()
            if (org == null) {
                if (auth.accessToken() == null) return // nothing persisted — stay SignedOut
                val orgs = auth.listOrgs() // pre-update session: re-derive the org
                when {
                    orgs.size == 1 -> org = orgs[0].organizationId
                    orgs.isEmpty() -> return
                    else -> {
                        _state.value = AppState.SelectingOrg(orgs)
                        return // selectOrg continues the flow from the picker
                    }
                }
            }
            orgId = org
            _state.value = AppState.Connecting
            try {
                auth.refreshSerialized()
            } catch (e: AppError.Auth) {
                _state.value = AppState.SignedOut // revoked/expired — back to login
                return
            } catch (_: Throwable) {
                // Transport/offline: keep the stored pair; the socket gate decides.
            }
            connectRegistry(org)
            _state.value = AppState.Ready
        } catch (e: Throwable) {
            _state.value = AppState.SignedOut
        }
    }

    /**
     * Back from the background: a dropped room used to stay dropped until the
     * user killed the app — rejoin (or kick) when we know we should be
     * connected but aren't.
     */
    fun onForeground() {
        if (_state.value is AppState.Ready && !registry.connected.value) registry.kick()
    }

    /**
     * "The network is back" (iOS OnlineBus): re-dial every live socket — the
     * registry room, the open session's chat2 room, and any parked attachment
     * escorts (they un-park on the same event via OnlineBus.waitBackoff).
     */
    fun onNetworkRestored() {
        OnlineBus.notifyOnline()
        if (_state.value is AppState.Ready && !registry.connected.value) registry.kick()
        val repo = session
        if (repo != null && !repo.connected.value) repo.kick()
    }

    /** Explicit "Try again" from the workspace's disconnected/error state. */
    fun retryRegistry() {
        val org = orgId ?: return
        viewModelScope.launch {
            runCatching { connectRegistry(org) }
                .onFailure { _state.value = AppState.Fatal(it.message ?: "reconnect failed") }
        }
    }

    private suspend fun connectRegistry(organizationId: String) {
        val token = auth.accessToken()
            ?: throw IllegalStateException("no access token")
        registry.start(
            cursor = null,
            deviceId = config.deviceId,
            url = EdgeConfig.registryWSUrl(organizationId, token, config.deviceId),
        )
    }

    fun openChat(id: String) {
        _selectedChat.value = id
        // iOS parity: a session's provider is set-once on the host (stamped on
        // the first run, never updated), so the harness is locked mid-session
        // while the model stays switchable within it — the static catalog
        // models every harness the fleet can produce (grok/hermes/pi included),
        // so the picker is never read-only for a configured session. Only a
        // config-less chat stays fully editable; its first run stamps the pair.
        // NOTE: named `chatConfig` — it must NOT shadow the AppConfig ctor
        // param `config` used below for deviceId.
        val chatConfig = chats.value.firstOrNull { it.id == id }?.config
        if (chatConfig != null) {
            _modelSelection.value = HarnessCatalog.Selection(chatConfig.harness ?: "", chatConfig.model, chatConfig.reasoning)
            _harnessLocked.value = true
        } else {
            _harnessLocked.value = false
        }
        // Live model catalog for the open session (iOS ComposerView `.task(id:
        // chat.id/harness)`): the host's own ListModels answer over the relay
        // — static fallback when the host is dark. The relay is shared with
        // the new-session flow; closeNewSession never races this load because
        // createSession closes the new-session flow before openChat runs.
        val hostDevice = chats.value.firstOrNull { it.id == id }?.deviceId
        val sessionHarness = chatConfig?.harness
        if (hostDevice != null && sessionHarness != null) {
            viewModelScope.launch {
                val relay = relayFor(hostDevice) ?: return@launch
                val models = runCatching { relay.listModels(sessionHarness).getOrThrow() }.getOrNull()
                    ?: return@launch
                _sessionCatalogs.value = mapOf(sessionHarness to models)
            }
        }
        // Tear down the previous session FIRST, then stamp this one: setting
        // sessionChatId before closeSession let it null the NEW id (it saves
        // the old one into closedChatId), so hostOnline()/setLive never saw
        // the open session — host presence and live badges silently no-oped.
        closeSession()
        sessionChatId = id
        _sessionStatus.value = SessionStatus.Connecting
        val doc = try { RealNativeLoroDoc() } catch (e: Throwable) {
            _sessionStatus.value = SessionStatus.Failed(e.message ?: "native doc failed")
            return
        }
        openDoc = doc
        val sync = ChatSync(id, OkHttpWebSocket(), http, doc)
        val repo = SessionRepository(id, doc, SessionAdapter(doc), sync, viewModelScope)
        session = repo
        startSendStatePulse(repo)

        viewModelScope.launch {
            val token = auth.accessToken()
            if (token == null) {
                _sessionStatus.value = SessionStatus.SignedOut
                return@launch
            }
            // Local-first (iOS DocDisk.loadChat2): the last-synced snapshot
            // renders instantly — offline included — and the join backfills
            // incrementally from its cursor instead of a full re-serve.
            val savedCursor = sh.zeron.android.data.DocDisk.loadChat2(doc, id)
            repo.start(
                cursor = savedCursor ?: 0,
                deviceId = config.deviceId,
                url = EdgeConfig.chat2WSUrl(id, token, config.deviceId),
                checkpointUrl = EdgeConfig.chat2CheckpointUrl(id, token),
            )
            launch {
                repo.transcript.collect {
                    _transcript.value = it
                    _openInputRequest.value = it.openInputRequest
                }
            }
            launch { repo.transferProgress.collect { _transferProgress.value = it } }
            launch {
                repo.connected.collect { on ->
                    if (on) _sessionStatus.value = SessionStatus.Connected
                    recomputeDeliveryDegraded()
                }
            }
            launch {
                repo.lastError.collect { e -> if (e != null) _sessionStatus.value = SessionStatus.Failed(e) }
            }
            launch {
                // The chip shows "older messages unavailable" only while the
                // checkpoint is genuinely missing; once it imports, revert to
                // Connected so the warning doesn't stick after recovery.
                repo.checkpointPending.collect { pending ->
                    _sessionStatus.value = when {
                        pending -> SessionStatus.HistoryTrimmed
                        repo.connected.value -> SessionStatus.Connected
                        else -> SessionStatus.Connecting
                    }
                }
            }
            repo.refresh()
        }
    }

    private fun closeSession() {
        val repo = session
        val closedChatId = sessionChatId
        session = null
        sessionChatId = null
        val doc = openDoc
        openDoc = null
        sendStateJob?.cancel()
        sendStateJob = null
        _sendState.value = null
        _transferProgress.value = null
        _sessionCatalogs.value = emptyMap()
        _openInputRequest.value = null
        recomputeDeliveryDegraded()
        // Fall back to the doc-derived truth for that chat's row badge.
        if (closedChatId != null) deliveryTracker.setLive(closedChatId, null)
        if (repo != null) viewModelScope.launch { repo.shutdown() } else doc?.close()
    }

    fun closeChat() {
        _selectedChat.value = null
        closeSession()
        _transcript.value = Transcript(emptyList())
        _openInputRequest.value = null
        _sessionStatus.value = SessionStatus.Connecting
        _sending.value = false
    }

    /**
     * 1Hz pulse while the session has pending sends (iOS ConnectivityCenter's
     * pulse): elapsed-based send states need a tick to flip Sending → Failed
     * at the 2-minute grace; a healthy idle session never repaints on it.
     */
    private fun startSendStatePulse(repo: SessionRepository) {
        sendStateJob?.cancel()
        sendStateJob = viewModelScope.launch {
            while (isActive) {
                val state: SendState? = if (repo.pendingSends.value.isNotEmpty()) {
                    repo.sendState(
                        now = System.currentTimeMillis(),
                        offline = Connectivity.offline.value,
                        hostOnline = hostOnline(),
                    )
                } else null
                _sendState.value = state
                // Keep the workspace row in sync while this session is open.
                sessionChatId?.let { id -> deliveryTracker.setLive(id, state) }
                delay(1_000)
            }
        }
    }

    /**
     * iOS chatDeliveryDegraded: every chat is remote-hosted on the phone, so
     * a send queues whenever the OS path is down, the chat2 room is down, or
     * the host device's presence is dark.
     */
    private fun recomputeDeliveryDegraded() {
        val repo = session
        val roomUp = repo == null || repo.connected.value
        _deliveryDegraded.value = Connectivity.offline.value || !roomUp || !hostOnline()
    }

    /** The open chat's host device is online while its presence beat is fresh. */
    private fun hostOnline(): Boolean {
        val deviceId = sessionChatId?.let { id ->
            chats.value.firstOrNull { it.id == id }?.deviceId
        } ?: return true
        val seen = presence.value[deviceId] ?: return false
        return System.currentTimeMillis() - seen < presenceTtlMs
    }

    /** The "Not delivered — tap to retry" affordance (iOS retryDelivery). */
    fun retryDelivery() {
        val repo = session ?: return
        viewModelScope.launch {
            runCatching { repo.retryDelivery() }
                .onFailure { _sessionStatus.value = SessionStatus.Failed(it.message ?: "retry failed") }
        }
    }

    /** Backgrounding hook: persist the open session's snapshot immediately. */
    fun flushToDisk() {
        viewModelScope.launch { runCatching { session?.flushToDisk() } }
    }

    fun openNewSession() { _newSessionOpen.value = true }

    // MARK: New space (iOS NewSpaceSheet)

    private val _newSpaceOpen = MutableStateFlow(false)
    val newSpaceOpen: StateFlow<Boolean> = _newSpaceOpen
    private val _folderListing = MutableStateFlow<FolderListing?>(null)
    val folderListing: StateFlow<FolderListing?> = _folderListing
    private val _folderLoading = MutableStateFlow(false)
    val folderLoading: StateFlow<Boolean> = _folderLoading
    private val _folderError = MutableStateFlow<String?>(null)
    val folderError: StateFlow<String?> = _folderError
    private val _spaceCreating = MutableStateFlow(false)
    val spaceCreating: StateFlow<Boolean> = _spaceCreating
    /** isRepo of the folder we're inside (iOS NewSpaceSheet.currentIsRepo). */
    private val _folderCurrentIsRepo = MutableStateFlow(false)
    val folderCurrentIsRepo: StateFlow<Boolean> = _folderCurrentIsRepo
    /** The device the folder browser is scoped to. */
    private val _folderDeviceId = MutableStateFlow<String?>(null)
    val folderDeviceId: StateFlow<String?> = _folderDeviceId

    fun openNewSpace() {
        _newSpaceOpen.value = true
        _folderListing.value = null
        _folderError.value = null
        _folderCurrentIsRepo.value = false
        val first = devices.value.firstOrNull { it.platform != "ios" && it.platform != "android" }
        if (first != null) pickFolderDevice(first.id) else _folderDeviceId.value = null
    }

    fun closeNewSpace() {
        _newSpaceOpen.value = false
        spaceRelay?.close()
        spaceRelay = null
        _folderListing.value = null
        _folderError.value = null
        _spaceCreating.value = false
        _folderCurrentIsRepo.value = false
        _folderDeviceId.value = null
    }

    /** Switch the folder browser's device tab (iOS NewSpaceSheet deviceTabs). */
    fun pickFolderDevice(deviceId: String) {
        if (_folderDeviceId.value == deviceId) return
        _folderDeviceId.value = deviceId
        loadFolders(deviceId, null, isRepo = false)
    }

    /** ListFolders on the picked device (nil path = its home). */
    fun loadFolders(deviceId: String, path: String?, isRepo: Boolean = false) {
        _folderCurrentIsRepo.value = isRepo
        viewModelScope.launch {
            _folderLoading.value = true
            _folderError.value = null
            val relay = spaceRelayFor(deviceId)
            val result = if (relay == null) null
            else runCatching { relay.listFolders(path).getOrThrow() }.getOrNull()
            _folderLoading.value = false
            if (result != null) {
                _folderListing.value = result
            } else if (_folderListing.value == null) {
                _folderError.value = "Couldn't reach the device — make sure it's online."
            }
        }
    }

    /**
     * Create a space (iOS NewSpaceSheet.create): preferred path is Mutate
     * {op:createSpace} straight to the owning host over its relay; a local
     * full-row upsert is the fallback when the host is unreachable. `onDone`
     * gets the new space id, or null on failure.
     */
    fun createSpace(deviceId: String, path: String, gitDetected: Boolean, onDone: (String?) -> Unit) {
        viewModelScope.launch {
            _spaceCreating.value = true
            val spaceId = UUID.randomUUID().toString().lowercase()
            val relay = spaceRelayFor(deviceId)
            val viaHost = relay != null &&
                runCatching { relay.mutateCreateSpace(spaceId, deviceId, path, gitDetected).getOrThrow() }
                    .getOrDefault(false)
            if (!viaHost) {
                registry.createSpace(spaceId, deviceId, path, gitDetected)
            }
            _spaceCreating.value = false
            onDone(spaceId)
        }
    }

    /** The relay for the folder-browser device — its own, to not disturb new-session RPCs. */
    private suspend fun spaceRelayFor(deviceId: String): DeviceRelayClient? {
        val token = auth.accessToken() ?: return null
        val existing = spaceRelay
        if (existing != null && existing.deviceId == deviceId && existing.isUsable) return existing
        existing?.close()
        val client = DeviceRelayClient(
            deviceId,
            EdgeConfig.relayWsUrl(deviceId, token),
            OkHttpWebSocket(),
            viewModelScope,
        )
        client.connect()
        spaceRelay = client
        return client
    }

    /**
     * Version gates (iOS WorkspaceStore.deviceVersionAtLeast): unknown device
     * or an unparsable/unstamped version reads as "too old" → legacy behavior.
     */
    fun deviceVersionAtLeast(deviceId: String?, min: Triple<Int, Int, Int>): Boolean {
        if (deviceId == null) return false
        val raw = devices.value.firstOrNull { it.id == deviceId }?.version ?: return false
        val parts = raw.split(".", "-").mapNotNull { it.toIntOrNull() }
        if (parts.size < 3) return false
        val v = Triple(parts[0], parts[1], parts[2])
        // Triple isn't Comparable — compare component-wise (iOS numeric compare).
        return v.first > min.first ||
            (v.first == min.first && v.second > min.second) ||
            (v.first == min.first && v.second == min.second && v.third >= min.third)
    }

    fun closeNewSession() {
        _newSessionOpen.value = false
        relay?.close()
        relay = null
        _newSessionRefs.value = null
        _newSessionRefsLoading.value = false
        _newSessionHarnesses.value = null
        _newSessionCatalogs.value = emptyMap()
    }

    /**
     * Load the space's host's live harness list + a model catalog per harness
     * (iOS NewSessionView `.task(id: spaceId)`): every catalog up front so the
     * sectioned picker can render without per-harness round trips. The static
     * pair stays until the list lands; a harness missing from the live list
     * falls back to its static catalog.
     */
    fun loadLiveCatalog(space: SpaceRow) {
        viewModelScope.launch {
            val relay = relayFor(space.deviceId) ?: return@launch
            val harnesses = runCatching { relay.listHarnesses().getOrThrow() }.getOrNull()
            if (harnesses == null) return@launch // offline — keep the static pair
            _newSessionHarnesses.value = harnesses
            val catalogs = mutableMapOf<String, List<ModelInfo>>()
            for (h in harnesses) {
                runCatching { relay.listModels(h.id).getOrThrow() }
                    .getOrNull()?.let { catalogs[h.id] = it }
            }
            _newSessionCatalogs.value = catalogs
        }
    }

    /**
     * iOS NewSessionView.send: mint the chat on the space's owning device
     * (the host runs it), then open it and queue the first prompt. The run is
     * a normal chat2 push — the host claims the row via the registry and
     * drains it; if the desktop is offline it fires when it reconnects.
     *
     * [branch] pins the chat to a base ref; [cwd] is the run folder (a reused
     * worktree path or the space folder); [worktree] is the WorktreeSpec the
     * host materializes at drain time (new-worktree plan).
     */
    fun createSession(
        spaceId: String,
        text: String,
        attachments: List<StagedAttachment> = emptyList(),
        branch: String? = null,
        cwd: String? = null,
        worktree: JSONObject? = null,
    ) {
        if (text.isBlank() && attachments.isEmpty()) return
        val space = registry.spaces.value.firstOrNull { it.id == spaceId } ?: return
        val selection = _modelSelection.value
        val config = JSONObject().put("harness", selection.harness).apply {
            selection.model?.let { put("model", it) }
            selection.reasoning?.let { put("reasoning", it) }
        }
        val chatId = registry.createChat(space, config, branch, cwd) ?: return
        closeNewSession() // clears the new-session state + closes the relay
        openChat(chatId)
        sendPromptWithAttachments(text, attachments, cwd, worktree)
    }

    /** Git refs of a new-session space, from its host device over the relay. */
    fun loadRefs(space: SpaceRow) {
        viewModelScope.launch {
            _newSessionRefsLoading.value = true
            _newSessionRefs.value = null
            val relay = relayFor(space.deviceId)
            val refs = if (relay == null) null
            else runCatching { relay.listRefs(space.path).getOrThrow() }.getOrNull()
            _newSessionRefs.value = refs
            _newSessionRefsLoading.value = false
        }
    }

    /**
     * SwitchRef on the host: git checkout in the space folder. `onDone` gets
     * the error message, or null on success (refs reload to refresh markers).
     */
    fun switchRef(space: SpaceRow, refName: String, onDone: (String?) -> Unit) {
        viewModelScope.launch {
            val relay = relayFor(space.deviceId)
            val err = when {
                relay == null -> "device offline"
                else -> runCatching { relay.switchRef(space.path, refName).getOrThrow() }
                    .fold({ null }, { it.message })
            }
            if (err == null) loadRefs(space)
            onDone(err)
        }
    }

    /** The relay for a space's host device — one per device, replaced on death. */
    private suspend fun relayFor(deviceId: String?): DeviceRelayClient? {
        if (deviceId == null) return null
        val token = auth.accessToken() ?: return null
        val existing = relay
        if (existing != null && existing.deviceId == deviceId && existing.isUsable) return existing
        existing?.close()
        val client = DeviceRelayClient(
            deviceId,
            EdgeConfig.relayWsUrl(deviceId, token),
            OkHttpWebSocket(),
            viewModelScope,
        )
        client.connect()
        relay = client
        return client
    }

    fun sendPrompt(text: String, cwd: String? = null, worktree: JSONObject? = null) =
        sendPromptWithAttachments(text, emptyList(), cwd, worktree)

    /**
     * iOS sendWithTransfers (queued flow, host ≥ 0.2.12): the command queues
     * IMMEDIATELY with `pending://{uploadId}/{name}` refs — a durable local
     * write — and the bytes chase it over the relay (retry-forever on the
     * online bus). The host defers the command until every ref's bytes land,
     * then rewrites the refs to absolute paths at dispatch. Legacy hosts
     * (< 0.2.12) stage attachments FIRST, so an upload failure aborts with
     * nothing queued (iOS NewSessionView.send).
     */
    fun sendPromptWithAttachments(text: String, attachments: List<StagedAttachment>, cwd: String? = null, worktree: JSONObject? = null) {
        if (text.isBlank() && attachments.isEmpty()) return
        val repo = session ?: return
        val chat = chats.value.firstOrNull { it.id == _selectedChat.value }
        val host = chat?.deviceId ?: return
        val selection = _modelSelection.value
        val prompt = text.trim()
        viewModelScope.launch {
            _sending.value = true
            try {
                if (deviceVersionAtLeast(host, queuedAttachmentsMin)) {
                    val transfers = attachments.map {
                        AttachmentTransfer(UUID.randomUUID().toString().lowercase(), it.name, it.data)
                    }
                    // Stash bytes FIRST — before anything references them.
                    transfers.forEach { UploadStash.save(it.uploadId, it.data) }
                    val refs = transfers.map { UploadStash.pendingRef(it.uploadId, it.name) }
                    repo.sendPrompt(
                        withAttachments(prompt, refs),
                        selection.harness, selection.model, selection.reasoning,
                        cwd, worktree, attachments = refs,
                    )
                    // Seed the just-sent bubble from local bytes.
                    for ((t, att) in transfers.zip(attachments)) {
                        AttachmentImageCache.seed(host, UploadStash.pendingRef(t.uploadId, t.name), att.name, att.data)
                    }
                    spawnEscort(repo, host, transfers)
                } else {
                    // Legacy host-staged path: upload everything first.
                    val paths = mutableListOf<String>()
                    for (att in attachments) {
                        val relay = relayFor(host) ?: throw IllegalStateException("host offline")
                        val path = uploadAttachmentChunked(relay, att.name, att.data) { frac ->
                            repo.setTransferProgress(frac)
                        }.getOrThrow()
                        AttachmentImageCache.seed(host, path, att.name, att.data)
                        paths += path
                    }
                    repo.setTransferProgress(null)
                    repo.sendPrompt(
                        withAttachments(prompt, paths),
                        selection.harness, selection.model, selection.reasoning,
                        cwd, worktree, attachments = paths,
                    )
                }
            } catch (e: Throwable) {
                repo.setTransferProgress(null)
                _sessionStatus.value = SessionStatus.Failed(e.message ?: "send failed")
            } finally {
                _sending.value = false
            }
        }
    }

    /**
     * Push a queued send's bytes to the host (iOS spawnEscort): retry-forever
     * (event-driven backoff, cut short by online events) up to the host's
     * 15-minute defer window. Success commits every upload; the command stays
     * durably queued in the doc no matter what happens here.
     */
    private fun spawnEscort(repo: SessionRepository, host: String, transfers: List<AttachmentTransfer>) {
        viewModelScope.launch {
            var pending = transfers.toList()
            var backoff = transferBackoffBaseMs
            val deadline = System.currentTimeMillis() + attachmentWaitMaxMs
            val totalBytes = maxOf(pending.sumOf { it.data.size }, 1)
            while (pending.isNotEmpty() && System.currentTimeMillis() < deadline && isActive) {
                val transfer = pending.first()
                val doneBytes = totalBytes - pending.sumOf { it.data.size }
                val relay = relayFor(host) ?: break
                val result = uploadAttachmentChunked(relay, transfer.name, transfer.data, transfer.uploadId) { frac ->
                    repo.setTransferProgress(minOf((doneBytes + frac * transfer.data.size) / totalBytes, 0.99))
                }
                if (result.isSuccess) {
                    UploadStash.delete(transfer.uploadId)
                    pending = pending.drop(1)
                    backoff = transferBackoffBaseMs
                } else {
                    OnlineBus.waitBackoff(backoff)
                    backoff = minOf(backoff * 2, transferBackoffCapMs)
                }
            }
            repo.setTransferProgress(null)
        }
    }

    /**
     * Transcript thumbnail load: ReadAttachmentChunk loop over the owning
     * device's relay → decode → seed the cache (iOS AttachmentImageCache
     * readImage). Failed loads retry on the 2s→15s ladder; a mutex keeps one
     * in-flight load per (device, path) from double-fetching.
     */
    fun loadAttachmentImage(deviceId: String, path: String) {
        viewModelScope.launch {
            attachmentLoadMutex.withLock {
                if (AttachmentImageCache.isLoadedOrLoading(deviceId, path)) return@withLock
                AttachmentImageCache.markLoading(deviceId, path)
                val relay = relayFor(deviceId) ?: run {
                    AttachmentImageCache.markError(deviceId, path, 0)
                    return@withLock
                }
                var name = ""
                val b64 = StringBuilder()
                var offset = 0L
                var done = false
                for (attempt in 0 until 1_000) {
                    val chunk = relay.readAttachmentChunk(path, offset).getOrNull() ?: break
                    if (chunk.name.isNotEmpty()) name = chunk.name
                    b64.append(chunk.data)
                    done = chunk.done
                    if (done) break
                    if (chunk.nextOffset <= offset) break // stuck-offset guard
                    offset = chunk.nextOffset
                }
                if (done) {
                    val bytes = runCatching { java.util.Base64.getDecoder().decode(b64.toString()) }.getOrNull()
                    if (bytes != null) {
                        AttachmentImageCache.store(deviceId, path, name.ifEmpty { path.substringAfterLast('/') }, decodeThumb(bytes), bytes.size)
                    } else {
                        AttachmentImageCache.markError(deviceId, path, 0)
                    }
                } else {
                    AttachmentImageCache.markError(deviceId, path, 0)
                }
            }
        }
    }

    private fun decodeThumb(bytes: ByteArray): android.graphics.Bitmap {
        val opts = android.graphics.BitmapFactory.Options().apply { inSampleSize = 2 }
        return android.graphics.BitmapFactory.decodeByteArray(bytes, 0, bytes.size, opts)
            ?: android.graphics.Bitmap.createBitmap(1, 1, android.graphics.Bitmap.Config.ARGB_8888)
    }

    /**
     * The composer's model picker: harness + model id (HarnessCatalog ids).
     * A locked provider rejects cross-harness picks (iOS `lockedHarness`). A
     * pick also rewrites the chat row's config (iOS `setChatConfig`), so the
     * next run on ANY device dispatches with it — not just this phone's next
     * send.
     */
    fun selectModel(harness: String, model: String) {
        if (_harnessLocked.value && harness != _modelSelection.value.harness) return // provider is fixed
        val catalog = modelsFor(harness)
        if (catalog.none { it.id == model }) return
        // iOS ModelPickerSheet.select: keep the current effort when the new
        // model's ladder has it, otherwise fall back to that model's default.
        val current = _modelSelection.value.reasoning
        val ladder = catalog.firstOrNull { it.id == model }?.reasoningLevels.orEmpty()
        val reasoning =
            if (current != null && current in ladder) current
            else HarnessCatalog.defaultReasoning(catalog.first { it.id == model })
        _modelSelection.value = HarnessCatalog.Selection(harness, model, reasoning)
        persistConfig(harness, model, reasoning)
    }

    /** The composer's picker catalog for a harness: the OPEN session's live
     *  catalog when loaded, else the new-session host's, else the static one. */
    private fun modelsFor(harness: String): List<ModelInfo> =
        _sessionCatalogs.value[harness]
            ?: _newSessionCatalogs.value[harness]
            ?: HarnessCatalog.models(harness)

    /** The composer's effort picker (iOS TraitPickerSheet) — run-level only. */
    fun selectReasoning(level: String) {
        _modelSelection.value =
            _modelSelection.value.copy(reasoning = level)
        persistConfig(
            _modelSelection.value.harness,
            _modelSelection.value.model,
            level,
        )
    }

    /**
     * iOS ComposerView.writeConfig: merge the pick into the chat row's config
     * field, preserving reasoning/modelOptions the desktop pickers set (a
     * blind rewrite would clobber them). The registry sync pushes it LWW.
     */
    private fun persistConfig(harness: String, model: String?, reasoning: String?) {
        val chatId = _selectedChat.value ?: return
        val existing = registry.chatConfig(chatId)
        val merged = JSONObject()
        if (existing != null) {
            val keys = existing.keys()
            while (keys.hasNext()) {
                val key = keys.next()
                merged.put(key, existing.get(key))
            }
        }
        merged.put("harness", harness)
        if (model != null) merged.put("model", model) else merged.remove("model")
        if (reasoning != null) merged.put("reasoning", reasoning) else merged.remove("reasoning")
        registry.setChatConfig(chatId, merged)
    }

    /** Ask the host to abandon the running turn (chat2 `interrupt` command). */
    fun interrupt() {
        val repo = session ?: return
        viewModelScope.launch {
            try {
                repo.interrupt()
            } catch (e: Throwable) {
                _sessionStatus.value = SessionStatus.Failed(e.message ?: "interrupt failed")
            }
        }
    }

    /**
     * Answer the agent's open question (iOS SessionStore.respondInput): one
     * [InputAnswer] per question, queued as a durable command the host drains.
     * The panel disappears once the host stamps the input part `resolved`.
     */
    fun answerInput(requestId: String, answers: List<InputAnswer>) {
        val repo = session ?: return
        viewModelScope.launch {
            runCatching { repo.respondInput(requestId, answers) }
                .onFailure { _sessionStatus.value = SessionStatus.Failed(it.message ?: "answer failed") }
        }
    }

    /**
     * WorkOS AuthKit: the caller opens `authorizeUrl(state)` in a browser tab;
     * the callback returns here with the code (iOS SignInView.signIn parity).
     */
    fun beginSignIn(): String {
        val state = java.util.UUID.randomUUID().toString()
        pendingState = state
        _state.value = AppState.SigningIn
        return WorkOsAuth.authorizeUrl(state)
    }

    /** Browser tab dismissed without a callback. */
    fun signInAborted() {
        if (_state.value is AppState.SigningIn) {
            pendingState = null
            _state.value = AppState.SignedOut
        }
    }

    /** Deep-link callback: exchange the code once, state must match. */
    fun onAuthCallback(uri: android.net.Uri) {
        val expected = pendingState ?: return
        val callback = sh.zeron.android.auth.DeepLinkHandler.parse(uri, expected)
        pendingState = null
        if (callback == null) {
            _state.value = AppState.Fatal("Callback missing code or state mismatch")
            return
        }
        viewModelScope.launch {
            _state.value = AppState.SigningIn
            try {
                val orgs = auth.signInWithCode(callback.code)
                _state.value =
                    if (orgs.isEmpty()) AppState.Fatal("No organizations on this account")
                    else AppState.SelectingOrg(orgs)
            } catch (e: Throwable) {
                _state.value = AppState.Fatal(e.message ?: "sign-in failed")
            }
        }
    }

    fun selectOrg(org: AuthOrg) {
        viewModelScope.launch {
            _state.value = AppState.Connecting
            try {
                auth.selectOrgAndRefresh(org.organizationId)
                orgId = org.organizationId
                connectRegistry(org.organizationId)
                _state.value = AppState.Ready
            } catch (e: Throwable) {
                _state.value = AppState.Fatal(e.message ?: "org select failed")
            }
        }
    }

    /** Leave the dead-end Fatal screen without needing to kill the app. */
    fun dismissFatal() {
        if (_state.value is AppState.Fatal) _state.value = AppState.SignedOut
    }

    fun signOut() {
        viewModelScope.launch {
            registry.stop()
            auth.signOut()
            closeSession()
            closeNewSpace()
            sh.zeron.android.data.DocDisk.wipeAll()
            UploadStash.sweep(0) // drop every staged byte
            orgId = null
            _selectedChat.value = null
            _transcript.value = Transcript(emptyList())
            _state.value = AppState.SignedOut
        }
    }
}
