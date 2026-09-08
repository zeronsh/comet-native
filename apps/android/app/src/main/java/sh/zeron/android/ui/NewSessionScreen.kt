package sh.zeron.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.RadioButtonDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.json.JSONObject
import sh.zeron.android.R
import sh.zeron.android.data.DeviceRow
import sh.zeron.android.data.HarnessCatalog
import sh.zeron.android.data.HarnessInfo
import sh.zeron.android.data.ModelInfo
import sh.zeron.android.data.RepoRef
import sh.zeron.android.data.SpaceRow
import sh.zeron.android.data.StagedAttachment
import sh.zeron.android.ui.session.Composer
import sh.zeron.android.ui.session.ComposerMode
import sh.zeron.android.ui.session.ComposerState
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

/** Where the session's work happens (iOS CheckoutKind). */
enum class CheckoutKind { Local, NewWorktree }

/** A device is online while its last presence beat is this fresh (iOS presenceTtlMs). */
private const val PRESENCE_TTL_MS = 30_000L

/**
 * New session (iOS NewSessionView parity): pick the space the session will run
 * in — a folder on a desktop device — then compose the first prompt. Sending
 * mints the chat on that host device, opens it, and queues the first run; an
 * offline desktop fires it when it reconnects. Git spaces add the
 * checkout/ref chips: run in the folder as-is, reuse an existing worktree, or
 * mint a fresh worktree off a picked base ref. The composer reuses the chat
 * composer, picker free because no session config exists yet.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewSessionScreen(
    spaces: List<SpaceRow>,
    devices: List<DeviceRow>,
    presence: Map<String, Long>,
    refs: List<RepoRef>?,
    refsLoading: Boolean,
    harnesses: List<HarnessInfo>?,
    catalogs: Map<String, List<ModelInfo>>,
    modelSelection: HarnessCatalog.Selection,
    onSelectModel: (String, String) -> Unit,
    onSelectReasoning: (String) -> Unit,
    onLoadRefs: (SpaceRow) -> Unit,
    onLoadLiveCatalog: (SpaceRow) -> Unit,
    onSwitchRef: (SpaceRow, String, (String?) -> Unit) -> Unit,
    onNewSpace: () -> Unit = {},
    onBack: () -> Unit,
    onCreate: (String, String, List<StagedAttachment>, String?, String?, JSONObject?) -> Unit,
) {
    var selectedSpaceId by rememberSaveable(spaces) { mutableStateOf(spaces.firstOrNull()?.id) }
    var selectedRef by rememberSaveable { mutableStateOf<String?>(null) }
    // Not saveable (enum) and fine to reset on rotation — iOS @State parity.
    var checkoutKind by remember { mutableStateOf(CheckoutKind.Local) }
    var showRefPicker by remember { mutableStateOf(false) }
    var showCheckoutPicker by remember { mutableStateOf(false) }
    var switching by remember { mutableStateOf(false) }
    var switchError by remember { mutableStateOf<String?>(null) }

    val space = spaces.firstOrNull { it.id == selectedSpaceId }
    val refRow: RepoRef? = refs?.firstOrNull { it.name == selectedRef }

    // Each space has its own refs + live catalog; reset and reload on change
    // (iOS `.task(id: spaceId)` — refs and harness list load in parallel).
    LaunchedEffect(selectedSpaceId) {
        selectedRef = null
        checkoutKind = CheckoutKind.Local
        switchError = null
        val selected = space
        if (selected != null) {
            if (selected.gitDetected) onLoadRefs(selected)
            onLoadLiveCatalog(selected)
        }
    }

    val checkoutLabel: String = when {
        checkoutKind == CheckoutKind.NewWorktree -> stringResource(R.string.new_session_new_worktree)
        refRow?.worktreePath != null -> stringResource(R.string.new_session_current_worktree)
        else -> stringResource(R.string.new_session_current_checkout)
    }
    val refLabel: String = when {
        // selectedRef is a delegated var — no smart cast, so use orEmpty().
        selectedRef == null -> stringResource(R.string.new_session_select_ref)
        checkoutKind == CheckoutKind.NewWorktree ->
            stringResource(R.string.new_session_from_ref, selectedRef.orEmpty())
        else -> selectedRef.orEmpty()
    }

    Scaffold(
        containerColor = ZeronColors.bg,
        contentWindowInsets = WindowInsets(0, 0, 0, 0),
        topBar = {
            Column {
                TopAppBar(
                    navigationIcon = {
                        IconButton(onClick = onBack) {
                            Icon(
                                painterResource(R.drawable.ic_arrow_back),
                                contentDescription = stringResource(R.string.new_session_back),
                                tint = ZeronColors.text,
                            )
                        }
                    },
                    title = {
                        Text(
                            stringResource(R.string.new_session_title),
                            style = MaterialTheme.typography.titleMedium,
                            color = ZeronColors.text,
                        )
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = ZeronColors.surface,
                        titleContentColor = ZeronColors.text,
                    ),
                )
                HorizontalDivider(color = ZeronColors.divider)
            }
        },
        bottomBar = {
            Column {
                HorizontalDivider(color = ZeronColors.divider)
                // iOS's "where-it-runs" scope row: checkout + ref chips above
                // the composer pill, git spaces only.
                if (space?.gitDetected == true) {
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState())
                            .padding(horizontal = ZeronSpacing.md, vertical = ZeronSpacing.xs),
                        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
                    ) {
                        Chip(label = checkoutLabel, onClick = { showCheckoutPicker = true })
                        Chip(
                            label = refLabel,
                            enabled = !refsLoading && refs != null,
                            loading = refsLoading && refs == null,
                            onClick = { showRefPicker = true },
                        )
                    }
                }
                Composer(
                    state = ComposerState(
                        mode = ComposerMode.Draft,
                        canSend = selectedSpaceId != null,
                    ),
                    onSend = { text, attachments ->
                        if (space == null) return@Composer
                        // iOS sends chat.cwd (the space folder or a reused
                        // worktree) — never "~" (the host's home); a new-worktree
                        // plan rides the run and the HOST materializes it,
                        // overriding cwd with the fresh checkout path.
                        val cwd: String = when (checkoutKind) {
                            CheckoutKind.NewWorktree -> space.path
                            CheckoutKind.Local -> refRow?.worktreePath ?: space.path
                        }
                        val worktree: JSONObject? = when (checkoutKind) {
                            CheckoutKind.NewWorktree -> JSONObject()
                                .put("repoPath", space.path)
                                .put("base", selectedRef ?: "HEAD")
                            CheckoutKind.Local -> null
                        }
                        onCreate(space.id, text, attachments, selectedRef, cwd, worktree)
                    },
                    onSteer = {},
                    onStop = {},
                    modelSelection = modelSelection,
                    harnessLocked = false,
                    harnesses = harnesses,
                    catalogs = catalogs,
                    onSelectModel = onSelectModel,
                    onSelectReasoning = onSelectReasoning,
                )
            }
        },
    ) { padding ->
        if (spaces.isEmpty()) {
            Box(
                Modifier.fillMaxSize().padding(padding),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    stringResource(R.string.new_session_no_spaces),
                    style = MaterialTheme.typography.bodyMedium,
                    color = ZeronColors.textMuted,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(horizontal = ZeronSpacing.xxl),
                )
            }
        } else {
            LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = PaddingValues(
                    start = ZeronSpacing.lg,
                    end = ZeronSpacing.lg,
                    top = padding.calculateTopPadding() + ZeronSpacing.sm,
                    bottom = padding.calculateBottomPadding() + ZeronSpacing.xl,
                ),
                verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
            ) {
                item(key = "header") {
                    Text(
                        stringResource(R.string.new_session_space_header).uppercase(),
                        style = MaterialTheme.typography.labelSmall,
                        color = ZeronColors.textFaint,
                        modifier = Modifier.padding(top = ZeronSpacing.sm, bottom = ZeronSpacing.xs),
                    )
                }
                items(spaces, key = { it.id }) { s ->
                    SpaceRowItem(
                        space = s,
                        subtitle = deviceTag(s, devices, presence)
                            ?: stringResource(R.string.new_session_runs_on),
                        selected = s.id == selectedSpaceId,
                        onClick = { selectedSpaceId = s.id },
                    )
                }
                item(key = "new-space") {
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .clip(MaterialTheme.shapes.medium)
                            .background(ZeronColors.surface)
                            .clickable(onClick = onNewSpace)
                            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
                    ) {
                        Text(
                            stringResource(R.string.new_session_new_space),
                            style = MaterialTheme.typography.bodyMedium,
                            color = ZeronColors.text,
                            fontWeight = FontWeight.Medium,
                        )
                        Icon(
                            painterResource(R.drawable.ic_add),
                            contentDescription = null,
                            tint = ZeronColors.textFaint,
                            modifier = Modifier.size(16.dp),
                        )
                    }
                }
            }
        }
    }

    if (showRefPicker && refs != null) {
        RefPickerSheet(
            refs = refs,
            selected = selectedRef,
            switching = switching,
            error = switchError,
            onPick = { row ->
                // iOS pickRef: a worktree'd ref reuses that checkout; a ref in
                // new-worktree mode (or the current one) just records; any
                // other plain ref in Local mode switches the folder first.
                when {
                    row.worktreePath != null -> {
                        selectedRef = row.name
                        checkoutKind = CheckoutKind.Local
                        showRefPicker = false
                    }
                    checkoutKind == CheckoutKind.NewWorktree || row.current -> {
                        selectedRef = row.name
                        showRefPicker = false
                    }
                    else -> {
                        if (space == null) return@RefPickerSheet
                        switching = true
                        switchError = null
                        onSwitchRef(space, row.name) { err ->
                            switching = false
                            if (err == null) {
                                selectedRef = row.name
                                showRefPicker = false
                            } else {
                                switchError = err
                            }
                        }
                    }
                }
            },
            onDismiss = { showRefPicker = false },
        )
    }
    if (showCheckoutPicker) {
        CheckoutPickerSheet(
            kind = checkoutKind,
            selectedRefHasWorktree = refRow?.worktreePath != null,
            onPick = { kind ->
                // iOS pickCheckout: dropping to Local with a plain non-current
                // ref picked drops the pick — the current branch takes over.
                if (kind == CheckoutKind.Local && checkoutKind == CheckoutKind.NewWorktree) {
                    val picked = refRow
                    if (picked != null && picked.worktreePath == null && !picked.current) {
                        selectedRef = refs?.firstOrNull { it.current }?.name
                    }
                }
                checkoutKind = kind
                showCheckoutPicker = false
            },
            onDismiss = { showCheckoutPicker = false },
        )
    }
}

/** A pill chip (checkout/ref), disabled while refs are loading. */
@Composable
private fun Chip(label: String, enabled: Boolean = true, loading: Boolean = false, onClick: () -> Unit) {
    Row(
        Modifier
            .clip(MaterialTheme.shapes.extraSmall)
            .background(ZeronColors.surfaceRaised)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = ZeronSpacing.md, vertical = ZeronSpacing.sm),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        Text(
            label,
            style = MaterialTheme.typography.labelMedium,
            color = if (enabled) ZeronColors.text else ZeronColors.textFaint,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        if (loading) {
            CircularProgressIndicator(
                modifier = Modifier.size(12.dp),
                color = ZeronColors.textFaint,
                strokeWidth = 1.5.dp,
            )
        }
    }
}

/**
 * The space's host tag (iOS HomeView.deviceTag): "@ name" when online,
 * "@ name · offline" when the last presence beat is stale, or null when no
 * device row is known yet (the caller shows the generic fallback line).
 */
private fun deviceTag(space: SpaceRow, devices: List<DeviceRow>, presence: Map<String, Long>): String? {
    val name = devices.firstOrNull { it.id == space.deviceId }?.name ?: return null
    val seen = presence[space.deviceId]
    val online = seen != null && System.currentTimeMillis() - seen < PRESENCE_TTL_MS
    return if (online) "@ $name" else "@ $name · offline"
}

/** One space: the folder path + the device that will run sessions in it. */
@Composable
private fun SpaceRowItem(space: SpaceRow, subtitle: String, selected: Boolean, onClick: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surface)
            .selectable(
                selected = selected,
                role = Role.RadioButton,
                onClick = onClick,
            )
            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.md),
    ) {
        RadioButton(
            selected = selected,
            onClick = null, // the whole row is the target
            colors = RadioButtonDefaults.colors(
                selectedColor = ZeronColors.accent,
                unselectedColor = ZeronColors.textFaint,
            ),
        )
        Column(Modifier.weight(1f)) {
            Text(
                space.path,
                style = MaterialTheme.typography.bodyLarge,
                color = ZeronColors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                subtitle,
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
            )
        }
        Box(
            Modifier
                .size(7.dp)
                .clip(CircleShape)
                .background(if (selected) ZeronColors.accent else ZeronColors.textFaint)
        )
    }
}

/** The base-ref picker: refs with current/worktree markers; switching runs inline. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun RefPickerSheet(
    refs: List<RepoRef>,
    selected: String?,
    switching: Boolean,
    error: String?,
    onPick: (RepoRef) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = ZeronColors.surface,
    ) {
        Column(Modifier.fillMaxWidth().padding(bottom = ZeronSpacing.xl)) {
            Text(
                stringResource(R.string.new_session_ref),
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
            )
            if (error != null) {
                Text(
                    error,
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.danger,
                    modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.xs),
                )
            }
            refs.forEach { ref ->
                val subtitle = when {
                    ref.current -> stringResource(R.string.new_session_ref_current)
                    ref.worktreePath != null -> stringResource(R.string.new_session_ref_worktree)
                    else -> null
                }
                val isSelected = ref.name == selected
                val busy = switching && ref.name == selected
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clip(MaterialTheme.shapes.small)
                        .clickable(enabled = !switching) { onPick(ref) }
                        .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(
                            ref.name,
                            style = MaterialTheme.typography.bodyMedium,
                            fontWeight = if (isSelected) FontWeight.SemiBold else FontWeight.Normal,
                            color = if (isSelected) ZeronColors.accent else ZeronColors.text,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        subtitle?.let {
                            Text(it, style = MaterialTheme.typography.labelSmall, color = ZeronColors.textFaint)
                        }
                    }
                    if (busy) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(16.dp),
                            color = ZeronColors.accent,
                            strokeWidth = 2.dp,
                        )
                    }
                }
            }
        }
    }
}

/** Where the session runs: the folder as-is, or a fresh isolated worktree. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun CheckoutPickerSheet(
    kind: CheckoutKind,
    selectedRefHasWorktree: Boolean,
    onPick: (CheckoutKind) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = ZeronColors.surface,
    ) {
        Column(Modifier.fillMaxWidth().padding(bottom = ZeronSpacing.xl)) {
            Text(
                stringResource(R.string.new_session_checkout),
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
            )
            CheckoutRow(
                title = if (selectedRefHasWorktree) stringResource(R.string.new_session_current_worktree)
                else stringResource(R.string.new_session_current_checkout),
                subtitle = if (selectedRefHasWorktree) stringResource(R.string.new_session_reuse_worktree)
                else stringResource(R.string.new_session_run_in_space),
                selected = kind == CheckoutKind.Local,
                onClick = { onPick(CheckoutKind.Local) },
            )
            CheckoutRow(
                title = stringResource(R.string.new_session_new_worktree),
                subtitle = stringResource(R.string.new_session_fresh_worktree),
                selected = kind == CheckoutKind.NewWorktree,
                onClick = { onPick(CheckoutKind.NewWorktree) },
            )
        }
    }
}

@Composable
private fun CheckoutRow(title: String, subtitle: String, selected: Boolean, onClick: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.small)
            .clickable(onClick = onClick)
            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                title,
                style = MaterialTheme.typography.bodyMedium,
                fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
                color = if (selected) ZeronColors.accent else ZeronColors.text,
            )
            Text(subtitle, style = MaterialTheme.typography.labelSmall, color = ZeronColors.textFaint)
        }
    }
}
