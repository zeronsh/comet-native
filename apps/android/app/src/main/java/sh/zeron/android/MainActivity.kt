package sh.zeron.android

import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.browser.customtabs.CustomTabsIntent
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import kotlinx.coroutines.runBlocking
import sh.zeron.android.auth.AuthStateMachine
import sh.zeron.android.auth.HttpAuthClient
import sh.zeron.android.config.EdgeConfig
import sh.zeron.android.config.WorkOsAuth
import sh.zeron.android.data.PersistentDeviceIdStore
import sh.zeron.android.data.SecureTokenStore
import sh.zeron.android.sync.Connectivity
import sh.zeron.android.sync.OkHttpTransport
import sh.zeron.android.sync.OkHttpWebSocket
import sh.zeron.android.sync.RegistrySync
import sh.zeron.android.ui.AppRoot
import sh.zeron.android.ui.AppViewModel
import sh.zeron.android.ui.theme.ZeronTheme

class MainActivity : ComponentActivity() {
    private val viewModel: AppViewModel by viewModels {
        object : ViewModelProvider.Factory {
            @Suppress("UNCHECKED_CAST")
            override fun <T : ViewModel> create(modelClass: Class<T>): T {
                val context = applicationContext
                val tokens = SecureTokenStore(context)
                val http = OkHttpTransport()
                val deviceIdStore = PersistentDeviceIdStore(context.getSharedPreferences("zeron", MODE_PRIVATE))
                val deviceId = runBlocking { deviceIdStore.getOrCreate() }
                val config = EdgeConfig.appConfig(deviceId)
                val auth = AuthStateMachine(HttpAuthClient(config, http), tokens)
                val registry = RegistrySync(OkHttpWebSocket(), http)
                return AppViewModel(auth, registry, http, config) as T
            }
        }
    }

    /** True while the AuthKit tab is in front, so onResume can detect a dismiss. */
    private var awaitingCallback = false
    /** Monitors the OS path so a network return re-dials every socket (OnlineBus). */
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            Connectivity.setPathOffline(false)
            viewModel.onNetworkRestored()
        }

        override fun onLost(network: Network) {
            Connectivity.setPathOffline(true)
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            val hasInternet = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            Connectivity.setPathOffline(!hasInternet)
            if (hasInternet) viewModel.onNetworkRestored()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Edge-to-edge, as the theme file documents (it drift from this comment
        // once: the call went missing and every screen under-drew — status bar
        // black band, and the IME PANNED the window on top of the composer's
        // own imePadding, bouncing the whole screen while typing).
        enableEdgeToEdge()
        NativeLoader.loadOnce()
        val connectivityManager = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        runCatching { connectivityManager.registerDefaultNetworkCallback(networkCallback) }
        setContent {
            ZeronTheme {
                val state by viewModel.state.collectAsState()
                val chats by viewModel.chats.collectAsState()
                val selected by viewModel.selectedChat.collectAsState()
                val newSessionOpen by viewModel.newSessionOpen.collectAsState()
                val spaces by viewModel.spaces.collectAsState()
                val devices by viewModel.devices.collectAsState()
                val presence by viewModel.presence.collectAsState()
                val refs by viewModel.newSessionRefs.collectAsState()
                val refsLoading by viewModel.newSessionRefsLoading.collectAsState()
                val harnesses by viewModel.newSessionHarnesses.collectAsState()
                val catalogs by viewModel.newSessionCatalogs.collectAsState()
                val newSpaceOpen by viewModel.newSpaceOpen.collectAsState()
                val folderListing by viewModel.folderListing.collectAsState()
                val folderLoading by viewModel.folderLoading.collectAsState()
                val folderError by viewModel.folderError.collectAsState()
                val spaceCreating by viewModel.spaceCreating.collectAsState()
                val folderCurrentIsRepo by viewModel.folderCurrentIsRepo.collectAsState()
                val folderDeviceId by viewModel.folderDeviceId.collectAsState()
                val transcript by viewModel.transcript.collectAsState()
                val registryConnected by viewModel.registryConnected.collectAsState()
                val registryError by viewModel.registryError.collectAsState()
                val sessionStatus by viewModel.sessionStatus.collectAsState()
                val sending by viewModel.sending.collectAsState()
                val sendState by viewModel.sendState.collectAsState()
                val transferProgress by viewModel.transferProgress.collectAsState()
                val modelSelection by viewModel.modelSelection.collectAsState()
                val harnessLocked by viewModel.harnessLocked.collectAsState()
                val sessionCatalogs by viewModel.sessionCatalogs.collectAsState()
                val openInputRequest by viewModel.openInputRequest.collectAsState()
                val offline by viewModel.offline.collectAsState()
                val deliveryDegraded by viewModel.deliveryDegraded.collectAsState()
                val deliveryBadges by viewModel.deliveryBadges.collectAsState()
                AppRoot(
                    state = state,
                    onLogIn = { launchAuthKit() },
                    onOrgSelect = { viewModel.selectOrg(it) },
                    chats = chats,
                    registryConnected = registryConnected,
                    registryError = registryError,
                    onOpenChat = { viewModel.openChat(it) },
                    selectedChat = selected,
                    transcript = transcript,
                    sessionStatus = sessionStatus,
                    sending = sending,
                    sendState = sendState,
                    transferProgress = transferProgress,
                    onRetryDelivery = { viewModel.retryDelivery() },
                    modelSelection = modelSelection,
                    harnessLocked = harnessLocked,
                    onSelectModel = { harness, model -> viewModel.selectModel(harness, model) },
                    onSelectReasoning = { level -> viewModel.selectReasoning(level) },
                    sessionCatalogs = sessionCatalogs,
                    openInputRequest = openInputRequest,
                    onAnswerInput = { requestId, answers -> viewModel.answerInput(requestId, answers) },
                    offline = offline,
                    deliveryDegraded = deliveryDegraded,
                    newSessionOpen = newSessionOpen,
                    spaces = spaces,
                    devices = devices,
                    presence = presence,
                    refs = refs ?: emptyList(),
                    refsLoading = refsLoading,
                    harnesses = harnesses,
                    catalogs = catalogs,
                    onNewSession = { viewModel.openNewSession() },
                    onCloseNewSession = { viewModel.closeNewSession() },
                    onLoadRefs = { viewModel.loadRefs(it) },
                    onLoadLiveCatalog = { viewModel.loadLiveCatalog(it) },
                    onSwitchRef = { space, ref, done -> viewModel.switchRef(space, ref, done) },
                    onCreateSession = { spaceId, text, attachments, branch, cwd, worktree ->
                        viewModel.createSession(spaceId, text, attachments, branch, cwd, worktree)
                    },
                    newSpaceOpen = newSpaceOpen,
                    folderListing = folderListing,
                    folderLoading = folderLoading,
                    folderError = folderError,
                    spaceCreating = spaceCreating,
                    folderCurrentIsRepo = folderCurrentIsRepo,
                    folderDeviceId = folderDeviceId,
                    onOpenNewSpace = { viewModel.openNewSpace() },
                    onCloseNewSpace = { viewModel.closeNewSpace() },
                    onPickFolderDevice = { deviceId -> viewModel.pickFolderDevice(deviceId) },
                    onNavigateFolder = { path, isRepo ->
                        val deviceId = viewModel.folderDeviceId.value
                        if (deviceId != null) viewModel.loadFolders(deviceId, path, isRepo)
                    },
                    onCreateSpace = { deviceId, path, isRepo, done ->
                        viewModel.createSpace(deviceId, path, isRepo, done)
                    },
                    onBack = { viewModel.closeChat() },
                    onSend = { text, attachments ->
                        if (attachments.isEmpty()) viewModel.sendPrompt(text)
                        else viewModel.sendPromptWithAttachments(text, attachments)
                    },
                    onStop = { viewModel.interrupt() },
                    onLoadAttachment = { deviceId, path -> viewModel.loadAttachmentImage(deviceId, path) },
                    attachmentDeviceId = selected?.let { id -> chats.firstOrNull { it.id == id }?.deviceId },
                    deliveryBadges = deliveryBadges,
                    onRetry = { viewModel.retryRegistry() },
                    onSignOut = { viewModel.signOut() },
                )
            }
        }
        handleAuthIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleAuthIntent(intent)
    }

    private fun handleAuthIntent(intent: Intent?) {
        val uri = intent?.data ?: return
        if (uri.scheme != WorkOsAuth.CALLBACK_SCHEME || uri.host != WorkOsAuth.CALLBACK_HOST) return
        awaitingCallback = false
        viewModel.onAuthCallback(uri)
    }

    /** Open WorkOS AuthKit in a Custom Tab; return arrives via zeron://callback. */
    private fun launchAuthKit() {
        val url = viewModel.beginSignIn()
        awaitingCallback = true
        CustomTabsIntent.Builder().build().launchUrl(this, Uri.parse(url))
    }

    override fun onResume() {
        super.onResume()
        if (awaitingCallback) {
            // Back on our activity with no callback intent = user dismissed the tab.
            awaitingCallback = false
            viewModel.signInAborted()
        }
        viewModel.onForeground()
    }

    override fun onStop() {
        super.onStop()
        // Backgrounding: persist the open session's snapshot immediately.
        viewModel.flushToDisk()
    }
}
