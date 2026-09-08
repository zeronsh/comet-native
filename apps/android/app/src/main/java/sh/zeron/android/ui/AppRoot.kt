package sh.zeron.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import org.json.JSONObject
import sh.zeron.android.auth.AuthOrg
import sh.zeron.android.data.ChatRow
import sh.zeron.android.data.DeviceRow
import sh.zeron.android.data.FolderListing
import sh.zeron.android.data.HarnessCatalog
import sh.zeron.android.data.HarnessInfo
import sh.zeron.android.data.InputAnswer
import sh.zeron.android.data.ModelInfo
import sh.zeron.android.data.Part
import sh.zeron.android.data.RepoRef
import sh.zeron.android.data.SpaceRow
import sh.zeron.android.data.StagedAttachment
import sh.zeron.android.data.Transcript
import sh.zeron.android.sync.AppState
import sh.zeron.android.sync.SendState
import sh.zeron.android.ui.session.Composer
import sh.zeron.android.ui.session.ComposerMode
import sh.zeron.android.ui.session.ComposerState
import sh.zeron.android.ui.session.InputRequestPanel
import sh.zeron.android.ui.session.SessionScreen
import sh.zeron.android.ui.transcript.TranscriptView
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing
import sh.zeron.android.ui.workspace.WorkspaceScreen

@Composable
fun AppRoot(
    state: AppState,
    onLogIn: () -> Unit,
    onOrgSelect: (AuthOrg) -> Unit,
    chats: List<ChatRow> = emptyList(),
    registryConnected: Boolean = true,
    registryError: String? = null,
    onOpenChat: (String) -> Unit = {},
    selectedChat: String? = null,
    transcript: Transcript = Transcript(emptyList()),
    sessionStatus: SessionStatus = SessionStatus.Connecting,
    sending: Boolean = false,
    sendState: SendState? = null,
    transferProgress: Double? = null,
    onRetryDelivery: () -> Unit = {},
    modelSelection: HarnessCatalog.Selection = HarnessCatalog.defaultSelection(),
    harnessLocked: Boolean = false,
    onSelectModel: (String, String) -> Unit = { _, _ -> },
    onSelectReasoning: (String) -> Unit = {},
    /** Live model catalogs for the open session (host ListModels). */
    sessionCatalogs: Map<String, List<ModelInfo>> = emptyMap(),
    /** The unresolved agent question — replaces the composer until answered. */
    openInputRequest: Part.Input? = null,
    onAnswerInput: (String, List<InputAnswer>) -> Unit = { _, _ -> },
    /** Pre-send honesty caption (iOS chatDeliveryDegraded). */
    offline: Boolean = false,
    deliveryDegraded: Boolean = false,
    newSessionOpen: Boolean = false,
    spaces: List<SpaceRow> = emptyList(),
    devices: List<DeviceRow> = emptyList(),
    presence: Map<String, Long> = emptyMap(),
    refs: List<RepoRef> = emptyList(),
    refsLoading: Boolean = false,
    harnesses: List<HarnessInfo>? = null,
    catalogs: Map<String, List<ModelInfo>> = emptyMap(),
    onNewSession: () -> Unit = {},
    onCloseNewSession: () -> Unit = {},
    onLoadRefs: (SpaceRow) -> Unit = {},
    onLoadLiveCatalog: (SpaceRow) -> Unit = {},
    onSwitchRef: (SpaceRow, String, (String?) -> Unit) -> Unit = { _, _, _ -> },
    onCreateSession: (String, String, List<StagedAttachment>, String?, String?, JSONObject?) -> Unit = { _, _, _, _, _, _ -> },
    newSpaceOpen: Boolean = false,
    folderListing: FolderListing? = null,
    folderLoading: Boolean = false,
    folderError: String? = null,
    spaceCreating: Boolean = false,
    folderCurrentIsRepo: Boolean = false,
    folderDeviceId: String? = null,
    onOpenNewSpace: () -> Unit = {},
    onCloseNewSpace: () -> Unit = {},
    onPickFolderDevice: (String) -> Unit = {},
    onNavigateFolder: (String?, Boolean) -> Unit = { _, _ -> },
    onCreateSpace: (String, String, Boolean, (String?) -> Unit) -> Unit = { _, _, _, _ -> },
    onBack: () -> Unit = {},
    onSend: (String, List<StagedAttachment>) -> Unit = { _, _ -> },
    onStop: () -> Unit = {},
    onRetry: () -> Unit = {},
    onSignOut: () -> Unit = {},
    onLoadAttachment: (String, String) -> Unit = { _, _ -> },
    attachmentDeviceId: String? = null,
    /** Per-chat delivery badges on the workspace rows (iOS HomeView sendBadge). */
    deliveryBadges: Map<String, SendState?> = emptyMap(),
) {
    if (newSpaceOpen) {
        NewSpaceScreen(
            devices = devices,
            presence = presence,
            listing = folderListing,
            loading = folderLoading,
            error = folderError,
            creating = spaceCreating,
            currentIsRepo = folderCurrentIsRepo,
            folderDeviceId = folderDeviceId,
            onPickDevice = { onPickFolderDevice(it) },
            onNavigate = { path, isRepo -> onNavigateFolder(path, isRepo) },
            onCreate = { deviceId, path, isRepo ->
                onCreateSpace(deviceId, path, isRepo) { onCloseNewSpace() }
            },
            onBack = onCloseNewSpace,
        )
        return
    }
    if (newSessionOpen) {
        NewSessionScreen(
            spaces = spaces,
            devices = devices,
            presence = presence,
            refs = refs,
            refsLoading = refsLoading,
            harnesses = harnesses,
            catalogs = catalogs,
            modelSelection = modelSelection,
            onSelectModel = onSelectModel,
            onSelectReasoning = onSelectReasoning,
            onLoadRefs = onLoadRefs,
            onLoadLiveCatalog = onLoadLiveCatalog,
            onSwitchRef = onSwitchRef,
            onNewSpace = onOpenNewSpace,
            onBack = onCloseNewSession,
            onCreate = onCreateSession,
        )
        return
    }
    if (selectedChat != null) {
        val chat = chats.firstOrNull { it.id == selectedChat }
        // "The model has not finished": the host's own entry status, plus the
        // window between queueing a prompt and the host adopting it (no doc
        // entry exists yet, but the turn is very much on its way).
        val running = sending ||
            transcript.working ||
            sendState == SendState.Sending ||
            sendState == SendState.Queued
        SessionScreen(
            title = chat?.title ?: selectedChat,
            status = sessionStatus,
            isArchived = chat?.archived == true,
            sendState = sendState,
            transferProgress = transferProgress,
            onRetryDelivery = onRetryDelivery,
            onBack = onBack,
            transcript = { padding ->
                TranscriptView(
                    transcript,
                    // The scaffold's inset is bars only — messages still need
                    // a gutter, or bubbles run edge to edge.
                    contentPadding = PaddingValues(
                        start = ZeronSpacing.lg,
                        end = ZeronSpacing.lg,
                        top = padding.calculateTopPadding() + ZeronSpacing.md,
                        bottom = padding.calculateBottomPadding() + ZeronSpacing.md,
                    ),
                    working = running,
                    attachmentDeviceId = attachmentDeviceId,
                    onLoadAttachment = onLoadAttachment,
                )
            },
            composer = {
                // The agent asked a question (iOS SessionView): the panel
                // replaces the composer until the host stamps it resolved.
                val request = openInputRequest
                if (request != null) {
                    InputRequestPanel(
                        questions = request.questions,
                        onAnswer = { answers -> onAnswerInput(request.id, answers) },
                    )
                } else {
                    Composer(
                        state = ComposerState(
                            mode = if (sending) ComposerMode.Sending else ComposerMode.Draft,
                            canSend = !sending,
                            running = running,
                        ),
                        onSend = { text, attachments -> onSend(text, attachments) },
                        onSteer = { text -> onSend(text, emptyList()) },
                        onStop = onStop,
                        modelSelection = modelSelection,
                        harnessLocked = harnessLocked,
                        catalogs = sessionCatalogs,
                        onSelectModel = onSelectModel,
                        onSelectReasoning = onSelectReasoning,
                        branch = chat?.branch,
                        offline = offline,
                        deliveryDegraded = deliveryDegraded,
                    )
                }
            },
        )
        return
    }
    when (state) {
        is AppState.SignedOut -> SignInScreen(onLogIn)
        is AppState.SigningIn -> SignInScreen(onLogIn, isLoading = true)
        is AppState.SelectingOrg -> OrgPickerScreen(state.orgs, onOrgSelect)
        is AppState.Ready -> WorkspaceScreen(
            chats = chats,
            spaces = spaces,
            connected = registryConnected,
            error = registryError,
            onOpen = onOpenChat,
            onNewSession = onNewSession,
            onNewSpace = onOpenNewSpace,
            onRetry = onRetry,
            onSignOut = onSignOut,
            badges = deliveryBadges,
        )
        is AppState.Fatal -> FatalScreen(state.message)
        else -> LoadingScreen()
    }
}

@Composable
private fun LoadingScreen() {
    Box(Modifier.fillMaxSize().background(ZeronColors.bg), contentAlignment = Alignment.Center) {
        Text("Connecting…", style = MaterialTheme.typography.bodyMedium, color = ZeronColors.textMuted)
    }
}

@Composable
private fun FatalScreen(msg: String) {
    Box(Modifier.fillMaxSize().background(ZeronColors.bg), contentAlignment = Alignment.Center) {
        Text(
            msg,
            style = MaterialTheme.typography.bodyMedium,
            color = ZeronColors.danger,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(32.dp),
        )
    }
}
