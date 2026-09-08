package sh.zeron.android.ui.session

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CheckboxDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.RadioButtonDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import sh.zeron.android.R
import sh.zeron.android.data.HarnessCatalog
import sh.zeron.android.data.HarnessInfo
import sh.zeron.android.data.InputAnswer
import sh.zeron.android.data.InputQuestion
import sh.zeron.android.data.ModelInfo
import sh.zeron.android.data.StagedAttachment
import sh.zeron.android.data.stageAttachment
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing
import kotlinx.coroutines.launch

enum class ComposerMode { Draft, Sending, Steering, Disabled }

data class ComposerState(
    val mode: ComposerMode = ComposerMode.Draft,
    val canSend: Boolean = false,
    /**
     * A turn is in flight on the host (doc entry still `streaming`, or a send
     * not yet adopted). The button is a Stop for as long as this holds — the
     * text field stays live so the next prompt can be drafted meanwhile.
     */
    val running: Boolean = false,
)

/**
 * Prompt composer — a rounded pill over the transcript, Send morphing to Stop
 * while a turn runs (the desktop/iOS Send→Steer→Stop shape).
 *
 * The draft is [rememberSaveable] so a rotation or a process death no longer
 * throws away a half-typed message.
 */
@Composable
fun Composer(
    state: ComposerState,
    onSend: (String, List<StagedAttachment>) -> Unit,
    onSteer: (String) -> Unit,
    onStop: () -> Unit,
    modelSelection: HarnessCatalog.Selection = HarnessCatalog.defaultSelection(),
    harnessLocked: Boolean = false,
    harnesses: List<HarnessInfo>? = null,
    catalogs: Map<String, List<ModelInfo>> = emptyMap(),
    onSelectModel: (String, String) -> Unit = { _, _ -> },
    onSelectReasoning: (String) -> Unit = {},
    /** Base ref the session is pinned to (iOS BranchContextChip). */
    branch: String? = null,
    /** OS path offline (iOS ConnectivityCenter.state == .offline). */
    offline: Boolean = false,
    /** Pre-send honesty: a send right now would queue (iOS chatDeliveryDegraded). */
    deliveryDegraded: Boolean = false,
    modifier: Modifier = Modifier,
) {
    var draft by rememberSaveable { mutableStateOf("") }
    var showModelPicker by remember { mutableStateOf(false) }
    var showTraitPicker by remember { mutableStateOf(false) }
    var attachments by remember { mutableStateOf<List<StagedAttachment>>(emptyList()) }
    var attachError by remember { mutableStateOf<String?>(null) }
    val context = LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    // Hoisted: stringResource is @Composable and the staging runs off-main.
    val attachFailedText = stringResource(R.string.attach_failed)
    val photoPicker = rememberLauncherForActivityResult(
        ActivityResultContracts.PickVisualMedia()
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        // Read + decode + transcode off the main thread (a 24 MB photo must
        // not jank the composer).
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val staged = runCatching {
                context.contentResolver.openInputStream(uri)?.use { stageAttachment(it.readBytes()) }
            }.getOrNull()
            if (staged != null) {
                attachments = attachments + staged
                attachError = null
            } else {
                attachError = attachFailedText
            }
        }
    }
    fun launchPhotoPicker() {
        if (attachments.size >= 8) return // iOS caps staged images at 8
        photoPicker.launch(
            PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)
        )
    }
    // Live device catalog wins; static fallback (iOS `models` computed).
    val modelLevels: List<String> = remember(modelSelection, catalogs) {
        catalogs[modelSelection.harness]
            ?.firstOrNull { it.id == modelSelection.model }?.reasoningLevels
            ?: HarnessCatalog.reasoningLevels(modelSelection.harness, modelSelection.model)
    }
    val sending = state.running || state.mode == ComposerMode.Sending
    // Drafting stays open while a turn runs: a long run must not lock the
    // keyboard for minutes. Only the submit is held back.
    val enabled = state.mode != ComposerMode.Disabled
    val canSubmit = draft.isNotBlank() && state.canSend && !sending

    fun submit() {
        if (!canSubmit) return
        if (state.mode == ComposerMode.Steering) {
            onSteer(draft)
        } else {
            onSend(draft, attachments)
        }
        draft = ""
        attachments = emptyList()
        attachError = null
    }

    Column(
        modifier
            .fillMaxWidth()
            .background(ZeronColors.bg)
            .navigationBarsPadding()
            .imePadding(),
    ) {
        HorizontalDivider(color = ZeronColors.divider)
        // Pre-send honesty (composer.rs degraded notice, iOS ComposerView):
        // one quiet caption line — the send still works, it just queues.
        if (deliveryDegraded) {
            Text(
                stringResource(
                    if (offline) R.string.composer_offline_notice
                    else R.string.composer_degraded_notice
                ),
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
                modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.xs),
            )
        }
        Row(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = ZeronSpacing.sm),
            horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
        ) {
            // iOS ComposerView chips: branch context | model | trait (effort).
            if (!branch.isNullOrEmpty()) {
                BranchChip(branch)
            }
            ModelPickerRow(
                label = HarnessCatalog.modelLabel(modelSelection.harness, modelSelection.model),
                onClick = { showModelPicker = true },
                modifier = Modifier.weight(1f),
            )
            val reasoning = modelSelection.reasoning
            if (reasoning != null && modelLevels.isNotEmpty()) {
                TraitPickerRow(
                    label = HarnessCatalog.reasoningLabel(reasoning),
                    onClick = { showTraitPicker = true },
                )
            }
        }
        if (attachments.isNotEmpty()) {
            AttachmentStrip(
                attachments = attachments,
                onRemove = { id -> attachments = attachments.filter { it.id != id } },
            )
        }
        if (attachError != null) {
                Text(
                    attachError.orEmpty(),
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.danger,
                    modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.xs),
                )
            }
            Row(
                Modifier.padding(
                    horizontal = ZeronSpacing.md,
                    vertical = ZeronSpacing.sm,
                ),
                verticalAlignment = Alignment.Bottom,
                horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
            ) {
                IconButton(
                    onClick = ::launchPhotoPicker,
                    enabled = enabled && attachments.size < 8,
                    colors = IconButtonDefaults.iconButtonColors(
                        containerColor = ZeronColors.surfaceRaised,
                        contentColor = ZeronColors.textMuted,
                        disabledContainerColor = ZeronColors.surfaceRaised,
                        disabledContentColor = ZeronColors.textFaint,
                    ),
                    modifier = Modifier.size(44.dp).clip(CircleShape),
                ) {
                    Icon(
                        painterResource(R.drawable.ic_attach),
                        contentDescription = stringResource(R.string.composer_attach),
                        modifier = Modifier.size(20.dp),
                    )
                }
                BasicTextField(
                value = draft,
                onValueChange = { draft = it },
                enabled = enabled,
                textStyle = MaterialTheme.typography.bodyLarge.copy(color = ZeronColors.text),
                cursorBrush = SolidColor(ZeronColors.accent),
                maxLines = 5,
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = { submit() }),
                decorationBox = { innerTextField ->
                    Box(
                        Modifier
                            .clip(MaterialTheme.shapes.extraLarge)
                            .background(ZeronColors.surface)
                            .heightIn(min = 44.dp)
                            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
                        contentAlignment = Alignment.CenterStart,
                    ) {
                        if (draft.isEmpty()) {
                            Text(
                                stringResource(R.string.composer_placeholder),
                                style = MaterialTheme.typography.bodyLarge,
                                color = ZeronColors.textFaint,
                            )
                        }
                        innerTextField()
                    }
                },
                modifier = Modifier.weight(1f),
            )
            SendButton(
                sending = sending,
                enabled = canSubmit,
                onSend = ::submit,
                onStop = onStop,
            )
        }
    }
    if (showModelPicker) {
        ModelPickerSheet(
            selection = modelSelection,
            lockedHarness = harnessLocked,
            harnesses = harnesses,
            catalogs = catalogs,
            onSelect = { harness, model ->
                onSelectModel(harness, model)
                showModelPicker = false
            },
            onDismiss = { showModelPicker = false },
        )
    }
    if (showTraitPicker) {
        TraitPickerSheet(
            levels = modelLevels,
            selected = modelSelection.reasoning,
            onSelect = { level ->
                onSelectReasoning(level)
                showTraitPicker = false
            },
            onDismiss = { showTraitPicker = false },
        )
    }
}

/**
 * The staged-image strip (iOS AttachmentStripView): 56pt thumbs, wrapped,
 * tap previews, an x button per thumb.
 */
@Composable
private fun AttachmentStrip(
    attachments: List<StagedAttachment>,
    onRemove: (String) -> Unit,
) {
    androidx.compose.foundation.lazy.LazyRow(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = ZeronSpacing.md, vertical = ZeronSpacing.xs),
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
    ) {
        items(attachments.size) { ix ->
            val att = attachments[ix]
            Box {
                Image(
                    bitmap = att.bitmap.asImageBitmap(),
                    contentDescription = att.name,
                    modifier = Modifier
                        .size(56.dp)
                        .clip(RoundedCornerShape(10.dp))
                        .aspectRatio(1f),
                )
                IconButton(
                    onClick = { onRemove(att.id) },
                    modifier = Modifier
                        .align(Alignment.TopEnd)
                        .size(18.dp)
                        .clip(CircleShape)
                        .background(androidx.compose.ui.graphics.Color.Black.copy(alpha = 0.65f)),
                ) {
                    Icon(
                        painterResource(R.drawable.ic_close),
                        contentDescription = stringResource(R.string.composer_remove_attachment),
                        tint = ZeronColors.text,
                        modifier = Modifier.size(10.dp),
                    )
                }
            }
        }
    }
}

/**
 * The session's pinned base ref (iOS BranchContextChip) — where the session
 * runs on the host. Read-only context, not a picker.
 */
@Composable
private fun BranchChip(branch: String) {
    Row(
        Modifier
            .padding(vertical = ZeronSpacing.xs)
            .clip(MaterialTheme.shapes.extraSmall)
            .background(ZeronColors.surfaceRaised)
            .padding(horizontal = ZeronSpacing.sm, vertical = ZeronSpacing.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        Icon(
            painterResource(R.drawable.ic_git_branch),
            contentDescription = null,
            tint = ZeronColors.textFaint,
            modifier = Modifier.size(12.dp),
        )
        Text(
            branch,
            style = MaterialTheme.typography.labelMedium,
            color = ZeronColors.textMuted,
            maxLines = 1,
            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
        )
    }
}

/**
 * One-line model selector above the input: the picked model with an expand
 * chevron — the sheet that follows is always interactive, the harness locked
 * mid-session but the model list fully pickable.
 */
@Composable
private fun ModelPickerRow(label: String, onClick: () -> Unit, modifier: Modifier = Modifier) {
    Row(
        modifier
            .padding(vertical = ZeronSpacing.xs)
            .clip(MaterialTheme.shapes.extraSmall)
            .clickable(onClick = onClick)
            .padding(horizontal = ZeronSpacing.sm, vertical = ZeronSpacing.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        Text(
            stringResource(R.string.model_picker_label).uppercase(),
            style = MaterialTheme.typography.labelSmall,
            color = ZeronColors.textFaint,
        )
        Text(
            label,
            style = MaterialTheme.typography.bodyMedium,
            color = ZeronColors.text,
            maxLines = 1,
            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
        )
        Icon(
            painterResource(R.drawable.ic_expand_more),
            contentDescription = stringResource(R.string.model_picker_title),
            tint = ZeronColors.textFaint,
            modifier = Modifier.size(16.dp),
        )
    }
}

/** The effort chip (iOS TraitPickerSheet trigger) — shown when the model has a ladder. */
@Composable
private fun TraitPickerRow(label: String, onClick: () -> Unit) {
    Row(
        Modifier
            .padding(vertical = ZeronSpacing.xs)
            .clip(MaterialTheme.shapes.extraSmall)
            .clickable(onClick = onClick)
            .padding(horizontal = ZeronSpacing.sm, vertical = ZeronSpacing.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyMedium,
            color = ZeronColors.text,
            maxLines = 1,
            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
        )
        Icon(
            painterResource(R.drawable.ic_expand_more),
            contentDescription = stringResource(R.string.trait_picker_title),
            tint = ZeronColors.textFaint,
            modifier = Modifier.size(16.dp),
        )
    }
}

/** The effort ladder in its own sheet (iOS TraitPickerSheet). */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun TraitPickerSheet(
    levels: List<String>,
    selected: String?,
    onSelect: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = ZeronColors.surface,
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(bottom = ZeronSpacing.xl),
        ) {
            Text(
                stringResource(R.string.trait_picker_title),
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
            )
            levels.forEach { level ->
                val isSelected = selected == level
                Column(
                    Modifier
                        .fillMaxWidth()
                        .clip(MaterialTheme.shapes.small)
                        .clickable { onSelect(level) }
                        .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
                    verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
                ) {
                    Text(
                        HarnessCatalog.reasoningLabel(level),
                        style = MaterialTheme.typography.bodyMedium,
                        fontWeight = if (isSelected) FontWeight.SemiBold else FontWeight.Normal,
                        color = if (isSelected) ZeronColors.accent else ZeronColors.text,
                    )
                    HarnessCatalog.reasoningHint(level)?.let {
                        Text(it, style = MaterialTheme.typography.labelSmall, color = ZeronColors.textFaint)
                    }
                }
            }
        }
    }
}

/**
 * The provider/model picker: one section per harness, the current model
 * highlighted. Selection is a (harness, model) pair that rides the next run.
 * With [lockedHarness] the sheet collapses to the session's harness (iOS
 * `lockedHarness`): no provider header, only its models — model pickable,
 * provider not.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ModelPickerSheet(
    selection: HarnessCatalog.Selection,
    lockedHarness: Boolean = false,
    harnesses: List<HarnessInfo>? = null,
    catalogs: Map<String, List<ModelInfo>> = emptyMap(),
    onSelect: (String, String) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = ZeronColors.surface,
    ) {
        // A filter over harness/model labels+ids (case-insensitive). The live
        // catalogs can be a dozen providers × dozens of models long; without
        // this the only way to find a model is to scroll (desktop ModelPicker
        // has the same field). Empties on open — the whole list is the default.
        var query by remember { mutableStateOf("") }
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(bottom = ZeronSpacing.xl),
        ) {
            Text(
                stringResource(R.string.model_picker_title),
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
            )
            TextField(
                value = query,
                onValueChange = { query = it },
                singleLine = true,
                placeholder = {
                    Text(stringResource(R.string.model_picker_search), color = ZeronColors.textFaint)
                },
                textStyle = MaterialTheme.typography.bodyMedium.copy(color = ZeronColors.text),
                colors = TextFieldDefaults.colors(
                    focusedContainerColor = ZeronColors.surfaceRaised,
                    unfocusedContainerColor = ZeronColors.surfaceRaised,
                    focusedIndicatorColor = ZeronColors.accent,
                    unfocusedIndicatorColor = ZeronColors.divider,
                ),
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.xs),
            )
            // iOS ModelPickerSheet.sections: a locked provider offers just the
            // session's harness (single section, no header); otherwise the
            // device's live harness list (static pair as fallback).
            val sections = if (lockedHarness) {
                listOf(HarnessInfo(selection.harness, HarnessCatalog.harnessLabel(selection.harness)))
            } else {
                harnesses ?: HarnessCatalog.harnesses
            }
            val needle = query.trim()
            var anyMatch = false
            sections.forEach { harness ->
                val harnessMatches = needle.isEmpty() ||
                    harness.label.contains(needle, ignoreCase = true) ||
                    harness.id.contains(needle, ignoreCase = true)
                val models = (catalogs[harness.id] ?: HarnessCatalog.models(harness.id)).filter { model ->
                    needle.isEmpty() || harnessMatches ||
                        model.label.contains(needle, ignoreCase = true) ||
                        model.id.contains(needle, ignoreCase = true)
                }
                if (models.isEmpty()) return@forEach
                anyMatch = true
                if (sections.size > 1) {
                    Text(
                        harness.label,
                        style = MaterialTheme.typography.labelSmall,
                        color = ZeronColors.textFaint,
                        modifier = Modifier.padding(
                            start = ZeronSpacing.lg,
                            top = ZeronSpacing.sm,
                            bottom = ZeronSpacing.xs,
                        ),
                    )
                }
                models.forEach { model ->
                    val selected = selection.harness == harness.id && selection.model == model.id
                    Column(
                        Modifier
                            .fillMaxWidth()
                            .clip(MaterialTheme.shapes.small)
                            .clickable { onSelect(harness.id, model.id) }
                            .padding(
                                horizontal = ZeronSpacing.lg,
                                vertical = ZeronSpacing.sm,
                            ),
                        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
                    ) {
                        Text(
                            model.label,
                            style = MaterialTheme.typography.bodyMedium,
                            fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
                            color = if (selected) ZeronColors.accent else ZeronColors.text,
                        )
                        model.description?.let {
                            Text(
                                it,
                                style = MaterialTheme.typography.labelSmall,
                                color = ZeronColors.textFaint,
                            )
                        }
                    }
                }
            }
            if (!anyMatch) {
                Text(
                    stringResource(R.string.model_picker_no_results, needle),
                    style = MaterialTheme.typography.bodyMedium,
                    color = ZeronColors.textFaint,
                    modifier = Modifier.padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.lg),
                )
            }
        }
    }
}

/**
 * One button, one touch target. The old version nested an IconButton inside a
 * painted 48.dp Box (two overlapping targets), and drew a bare progress ring
 * while sending — so "Stop" announced itself to TalkBack but could not be
 * tapped at all.
 */
@Composable
private fun SendButton(
    sending: Boolean,
    enabled: Boolean,
    onSend: () -> Unit,
    onStop: () -> Unit,
) {
    val active = sending || enabled
    IconButton(
        onClick = { if (sending) onStop() else onSend() },
        enabled = active,
        colors = IconButtonDefaults.iconButtonColors(
            containerColor = if (active) ZeronColors.text else ZeronColors.surfaceRaised,
            contentColor = if (active) ZeronColors.bg else ZeronColors.textFaint,
            disabledContainerColor = ZeronColors.surfaceRaised,
            disabledContentColor = ZeronColors.textFaint,
        ),
        modifier = Modifier.size(44.dp).clip(CircleShape),
    ) {
        if (sending) {
            Box(contentAlignment = Alignment.Center) {
                CircularProgressIndicator(
                    modifier = Modifier.size(22.dp),
                    color = ZeronColors.bg,
                    strokeWidth = 2.dp,
                )
                Icon(
                    painterResource(R.drawable.ic_stop),
                    contentDescription = stringResource(R.string.composer_stop),
                    modifier = Modifier.size(10.dp),
                )
            }
        } else {
            Icon(
                painterResource(R.drawable.ic_arrow_upward),
                contentDescription = stringResource(R.string.composer_send),
                modifier = Modifier.size(20.dp),
            )
        }
    }
}

/**
 * The agent asked a question with fixed choices (iOS QuestionPanel): one
 * paged question at a time, radio (single) or checkbox (multi) options plus
 * a typed-answer fallback, Back/Next/Submit. Submitting answers every
 * question — picked labels, or the typed text when one was entered.
 */
@Composable
fun InputRequestPanel(
    questions: List<InputQuestion>,
    onAnswer: (List<InputAnswer>) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (questions.isEmpty()) return
    // Picked/typed maps aren't saveable; they reset on rotation like iOS @State.
    var page by rememberSaveable { mutableStateOf(0) }
    var picked by remember { mutableStateOf<Map<String, Set<String>>>(emptyMap()) }
    var typed by remember { mutableStateOf<Map<String, String>>(emptyMap()) }
    val question = questions[page.coerceAtMost(questions.size - 1)]
    val isPicked = picked[question.id].orEmpty()
    val typedText = typed[question.id].orEmpty()
    val canAdvance = typedText.isNotBlank() || isPicked.isNotEmpty()

    Column(
        modifier
            .fillMaxWidth()
            .padding(horizontal = ZeronSpacing.md, vertical = ZeronSpacing.sm)
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surfaceRaised)
            .padding(ZeronSpacing.lg),
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                question.header.uppercase(),
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
                modifier = Modifier.weight(1f),
            )
            if (questions.size > 1) {
                Text(
                    "${page + 1}/${questions.size}",
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.textFaint,
                )
            }
        }
        Text(question.question, style = MaterialTheme.typography.bodyLarge, color = ZeronColors.text)
        if (question.multiSelect) {
            Text(
                stringResource(R.string.input_request_multi_hint),
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
            )
        }
        question.options.forEach { option ->
            val selected = option in isPicked
            Row(
                Modifier
                    .fillMaxWidth()
                    .clip(MaterialTheme.shapes.small)
                    .selectable(
                        selected = selected,
                        role = if (question.multiSelect) Role.Checkbox else Role.RadioButton,
                        onClick = {
                            // Picking an option clears any typed answer (iOS pick()).
                            typed = typed - question.id
                            picked = if (question.multiSelect) {
                                val next = if (selected) isPicked - option else isPicked + option
                                picked + (question.id to next)
                            } else {
                                picked + (question.id to setOf(option))
                            }
                        },
                    )
                    .padding(vertical = ZeronSpacing.xs, horizontal = ZeronSpacing.sm),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
            ) {
                if (question.multiSelect) {
                    Checkbox(
                        checked = selected,
                        onCheckedChange = null, // the whole row is the target
                        colors = CheckboxDefaults.colors(
                            checkedColor = ZeronColors.accent,
                            uncheckedColor = ZeronColors.textFaint,
                        ),
                    )
                } else {
                    RadioButton(
                        selected = selected,
                        onClick = null, // the whole row is the target
                        colors = RadioButtonDefaults.colors(
                            selectedColor = ZeronColors.accent,
                            unselectedColor = ZeronColors.textFaint,
                        ),
                    )
                }
                Text(
                    option,
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (selected) ZeronColors.text else ZeronColors.textMuted,
                )
            }
        }
        // "Or type your own answer" — the typed text wins over the picks.
        TextField(
            value = typedText,
            onValueChange = { typed = typed + (question.id to it) },
            placeholder = {
                Text(stringResource(R.string.input_request_typed), color = ZeronColors.textFaint)
            },
            singleLine = true,
            textStyle = MaterialTheme.typography.bodyMedium.copy(color = ZeronColors.text),
            colors = TextFieldDefaults.colors(
                focusedContainerColor = ZeronColors.surface,
                unfocusedContainerColor = ZeronColors.surface,
                focusedIndicatorColor = ZeronColors.accent,
                unfocusedIndicatorColor = ZeronColors.divider,
            ),
            modifier = Modifier.fillMaxWidth(),
        )
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm, Alignment.End),
        ) {
            if (page > 0) {
                TextButton(onClick = { page -= 1 }) {
                    Text(stringResource(R.string.input_request_back), color = ZeronColors.textMuted)
                }
            }
            Button(
                onClick = {
                    if (page < questions.size - 1) {
                        page += 1
                    } else {
                        onAnswer(questions.map { q ->
                            val labels = typed[q.id]?.takeIf { it.isNotBlank() }
                                ?.let { listOf(it) }
                                ?: picked[q.id]?.toList()
                                ?: emptyList()
                            InputAnswer(q.id, labels)
                        })
                    }
                },
                enabled = canAdvance,
                colors = ButtonDefaults.buttonColors(
                    containerColor = ZeronColors.text,
                    contentColor = ZeronColors.bg,
                    disabledContainerColor = ZeronColors.surface,
                    disabledContentColor = ZeronColors.textFaint,
                ),
            ) {
                Text(
                    stringResource(
                        if (page < questions.size - 1) R.string.input_request_next
                        else R.string.input_request_submit
                    )
                )
            }
        }
    }
}
