package sh.zeron.android.ui.transcript

import android.content.ClipData
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.animateScrollBy
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.toClipEntry
import androidx.compose.ui.window.Dialog
import kotlinx.coroutines.launch
import sh.zeron.android.R
import sh.zeron.android.data.AttachmentImageCache
import sh.zeron.android.data.MessageRole
import sh.zeron.android.data.Part
import sh.zeron.android.data.Transcript
import sh.zeron.android.data.TranscriptMessage
import sh.zeron.android.data.UserImageAttachment
import sh.zeron.android.data.parseUserMessageImages
import sh.zeron.android.ui.theme.MonoStyle
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

/**
 * The conversation. Messages carry their author, so a prompt reads as a prompt:
 * the user's turn sits in a raised bubble on the trailing edge, the agent's runs
 * full-bleed. Before this, every part rendered identically and you could not
 * tell who had said what.
 */
@Composable
fun TranscriptView(
    transcript: Transcript,
    modifier: Modifier = Modifier,
    contentPadding: PaddingValues = PaddingValues(ZeronSpacing.lg),
    working: Boolean = false,
    attachmentDeviceId: String? = null,
    onLoadAttachment: (String, String) -> Unit = { _, _ -> },
) {
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()

    // Tool calls are host bookkeeping, not conversation: they are dropped here
    // rather than in the adapter so delivery tracking still sees every entry.
    // A message left with nothing to show drops out with them.
    val messages = remember(transcript.messages) {
        transcript.messages.mapNotNull { message ->
            val visible = message.parts.filterNot { it is Part.Tool }
            if (visible.isEmpty()) null else message.copy(parts = visible)
        }
    }
    val lastMessageId = messages.lastOrNull()?.id
    // Counted from the DATA, not layoutInfo: the first scroll runs before the
    // list has ever been measured, and layoutInfo still reports zero items.
    val lastIndex = messages.size - 1 + if (working) 1 else 0

    // Follow the tail only when the reader is already there. The old version
    // scrolled on every change, yanking you back down while reading history.
    val atBottom by remember { derivedStateOf { listState.isAtBottom() } }
    val atTop by remember {
        derivedStateOf { listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset == 0 }
    }

    // Opening a session lands at the newest message, unanimated — the reader
    // should never watch the history fly past to get where they were.
    var landed by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(messages.isNotEmpty()) {
        if (!landed && messages.isNotEmpty()) {
            listState.scrollToEnd(lastIndex, animated = false)
            landed = true
        }
    }

    // Streaming tail: follow the END of the last item — its pixel height, not
    // the part count. The part count steps once per whole text/reasoning part,
    // while streaming rewrites the tail part's text several times a second with
    // no count change — so the count-keyed effect stayed idle and the feed
    // stopped following mid-answer. Watching the item's total size also brings
    // in each growth INSIDE a tall part (markdown paragraphs, code blocks).
    val partCount = messages.sumOf { it.parts.size }

    // A finger on the list wins: reading/scanning history must not fight the
    // auto-follow. Drag events only — a programmatic follow is not an
    // interaction, so it never pauses itself.
    var dragging by remember { mutableStateOf(false) }
    LaunchedEffect(listState) {
        listState.interactionSource.interactions.collect { interaction ->
            when (interaction) {
                is androidx.compose.foundation.interaction.DragInteraction.Start -> dragging = true
                is androidx.compose.foundation.interaction.DragInteraction.Stop,
                is androidx.compose.foundation.interaction.DragInteraction.Cancel -> dragging = false
            }
        }
    }

    LaunchedEffect(partCount, working, lastMessageId) {
        if (!landed || !working || messages.isEmpty()) return@LaunchedEffect
        snapshotFlow { listState.layoutInfo.visibleItemsInfo
            .lastOrNull { it.index == lastIndex }?.let { it.offset + it.size } ?: 0 }
            .collect { tailBottom ->
                if (tailBottom > 0 && atBottom && !dragging) {
                    listState.scrollToEnd(lastIndex)
                }
            }
    }

    Box(modifier) {
        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxSize(),
            contentPadding = contentPadding,
            verticalArrangement = Arrangement.spacedBy(ZeronSpacing.lg),
        ) {
            items(messages, key = { it.id }) {
                MessageBlock(
                    message = it,
                    streaming = working && it.id == lastMessageId,
                    attachmentDeviceId = attachmentDeviceId,
                    onLoadAttachment = onLoadAttachment,
                )
            }
            if (working) item(key = "working-indicator") { WorkingIndicator() }
        }
        ScrollControls(
            showUp = !atTop && messages.isNotEmpty(),
            showDown = !atBottom && messages.isNotEmpty(),
            onUp = { scope.launch { listState.animateScrollToItem(0) } },
            onDown = { scope.launch { listState.scrollToEnd(lastIndex) } },
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(
                    end = ZeronSpacing.lg,
                    bottom = contentPadding.calculateBottomPadding() + ZeronSpacing.lg,
                ),
        )
    }
}

/**
 * True bottom, not "the last item is visible": a final assistant message taller
 * than the viewport is one item, so index-only checks call a reader parked at
 * its first paragraph "at the bottom" and then yank them down as it streams.
 */
private fun LazyListState.isAtBottom(): Boolean {
    val info = layoutInfo
    val last = info.visibleItemsInfo.lastOrNull() ?: return true
    if (last.index < info.totalItemsCount - 1) return false
    return last.offset + last.size <= info.viewportEndOffset - info.afterContentPadding + 4
}

/** Scroll to the END of item [target], past its own overflow. */
private suspend fun LazyListState.scrollToEnd(target: Int, animated: Boolean = true) {
    if (target < 0) return
    if (animated) animateScrollToItem(target) else scrollToItem(target)
    // animateScrollToItem lands the item's TOP edge; a taller-than-viewport
    // message still needs its tail brought into view.
    val info = layoutInfo
    val item = info.visibleItemsInfo.lastOrNull { it.index == target } ?: return
    val overflow = (item.offset + item.size) - (info.viewportEndOffset - info.afterContentPadding)
    if (overflow > 0) {
        if (animated) animateScrollBy(overflow.toFloat()) else scrollBy(overflow.toFloat())
    }
}

/**
 * Jump-to-edge buttons, floating over the feed's trailing edge. Each shows only
 * when it would do something — at the bottom of a fresh session neither is in
 * the way of the newest message.
 */
@Composable
private fun ScrollControls(
    showUp: Boolean,
    showDown: Boolean,
    onUp: () -> Unit,
    onDown: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier,
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
        horizontalAlignment = Alignment.End,
    ) {
        AnimatedVisibility(showUp, enter = fadeIn() + scaleIn(), exit = fadeOut() + scaleOut()) {
            ScrollButton(R.drawable.ic_arrow_upward, R.string.session_scroll_top, onUp)
        }
        AnimatedVisibility(showDown, enter = fadeIn() + scaleIn(), exit = fadeOut() + scaleOut()) {
            ScrollButton(R.drawable.ic_arrow_downward, R.string.session_scroll_bottom, onDown)
        }
    }
}

@Composable
private fun ScrollButton(
    @DrawableRes icon: Int,
    @StringRes description: Int,
    onClick: () -> Unit,
) {
    IconButton(
        onClick = onClick,
        colors = IconButtonDefaults.iconButtonColors(
            containerColor = ZeronColors.surfaceRaised,
            contentColor = ZeronColors.textMuted,
        ),
        modifier = Modifier
            .size(36.dp)
            .clip(CircleShape)
            .border(1.dp, ZeronColors.border, CircleShape),
    ) {
        Icon(
            painterResource(icon),
            contentDescription = stringResource(description),
            modifier = Modifier.size(16.dp),
        )
    }
}

/**
 * The turn is still being written (schema.rs `MessageStatus::Streaming`).
 * Three breathing dots at the tail of the feed — the composer's Send button
 * carries the same truth as a Stop.
 */
@Composable
private fun WorkingIndicator() {
    val transition = rememberInfiniteTransition(label = "working")
    Row(
        Modifier.padding(vertical = ZeronSpacing.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        repeat(3) { i ->
            val alpha by transition.animateFloat(
                initialValue = 0.2f,
                targetValue = 1f,
                animationSpec = infiniteRepeatable(
                    animation = tween(560, delayMillis = i * 160, easing = LinearEasing),
                    repeatMode = RepeatMode.Reverse,
                ),
                label = "workingDot$i",
            )
            Box(
                Modifier
                    .size(6.dp)
                    .clip(CircleShape)
                    .background(ZeronColors.textMuted.copy(alpha = alpha))
            )
        }
        Text(
            stringResource(R.string.session_working),
            style = MaterialTheme.typography.labelSmall,
            color = ZeronColors.textFaint,
            modifier = Modifier.padding(start = ZeronSpacing.xs),
        )
    }
}

@Composable
private fun MessageBlock(
    message: TranscriptMessage,
    streaming: Boolean,
    attachmentDeviceId: String?,
    onLoadAttachment: (String, String) -> Unit,
) {
    if (message.role == MessageRole.User) {
        Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.End) {
            Box(Modifier.fillMaxWidth(0.85f), contentAlignment = Alignment.CenterEnd) {
                Column(
                    Modifier
                        .clip(RoundedCornerShape(18.dp, 18.dp, 4.dp, 18.dp))
                        .background(ZeronColors.surfaceRaised)
                        .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
                    verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
                ) {
                    message.parts.forEach { part ->
                        // User text rides the attachment-ref trailer (iOS
                        // parseUserMessageImages) — split it and render thumbs.
                        if (part is Part.Text && attachmentDeviceId != null) {
                            UserTextWithAttachments(part.text, attachmentDeviceId, onLoadAttachment)
                        } else {
                            PartView(part, streaming = false)
                        }
                    }
                }
            }
            // Copy sits OUTSIDE the bubble (aligned with it): inside it the
            // long-press that starts text selection pelearía con el drag de
            // la lista. Copia el texto plano del mensaje entero.
            CopyButton(
                plainText = message.parts
                    .filterIsInstance<Part.Text>()
                    .map { it.text }
                    .joinToString("\n\n"),
            )
        }
        return
    }
    Column(
        Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
    ) {
        // Only the tail part of a live turn is "still arriving" — an earlier
        // thought in the same message is already settled and stays collapsed.
        val tail = message.parts.lastOrNull()
        message.parts.forEach { PartView(it, streaming = streaming && it === tail) }
        CopyButton(
            plainText = message.parts
                .filterIsInstance<Part.Text>()
                .map { it.text }
                .joinToString("\n\n"),
        )
    }
}

/** User text + any parsed attachment thumbnails (112×80, right-aligned strip). */
@Composable
private fun UserTextWithAttachments(
    content: String,
    deviceId: String,
    onLoadAttachment: (String, String) -> Unit,
) {
    val parsed = remember(content) { parseUserMessageImages(content) }
    if (parsed.attachments.isEmpty()) {
        MarkdownText(content)
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm)) {
        if (parsed.text.isNotEmpty()) MarkdownText(parsed.text)
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm, Alignment.End),
        ) {
            parsed.attachments.forEach { att ->
                AttachmentThumb(deviceId, att, onLoadAttachment)
            }
        }
    }
}

/** One transcript thumbnail: loading spinner → loaded image → tap for full view. */
@Composable
private fun AttachmentThumb(
    deviceId: String,
    att: UserImageAttachment,
    onLoadAttachment: (String, String) -> Unit,
) {
    var preview by rememberSaveable(att.path) { mutableStateOf(false) }
    val snapshot = AttachmentImageCache.snapshot(deviceId, att.path)
    LaunchedEffect(deviceId, att.path) {
        if (snapshot !is AttachmentImageCache.Snapshot.Loaded) {
            onLoadAttachment(deviceId, att.path)
        }
    }
    Box(
        Modifier
            .size(width = 112.dp, height = 80.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(ZeronColors.surface)
            .border(1.dp, ZeronColors.border, RoundedCornerShape(8.dp)),
        contentAlignment = Alignment.Center,
    ) {
        when (snapshot) {
            is AttachmentImageCache.Snapshot.Loaded -> {
                Image(
                    bitmap = snapshot.bitmap.asImageBitmap(),
                    contentDescription = att.name,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier
                        .fillMaxSize()
                        .clip(RoundedCornerShape(8.dp))
                        .clickable { preview = true },
                )
            }
            is AttachmentImageCache.Snapshot.Error -> {
                Text(
                    stringResource(R.string.attachment_error),
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.textFaint,
                    modifier = Modifier.clickable { onLoadAttachment(deviceId, att.path) },
                )
            }
            else -> {
                CircularProgressIndicator(
                    modifier = Modifier.size(16.dp),
                    color = ZeronColors.textFaint,
                    strokeWidth = 2.dp,
                )
            }
        }
    }
    if (preview) {
        val loaded = snapshot as? AttachmentImageCache.Snapshot.Loaded
        if (loaded != null) {
            Dialog(onDismissRequest = { preview = false }) {
                Box(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(8.dp))
                        .background(androidx.compose.ui.graphics.Color.Black.copy(alpha = 0.9f))
                        .clickable { preview = false },
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        Modifier.padding(ZeronSpacing.lg),
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
                    ) {
                        Image(
                            bitmap = loaded.bitmap.asImageBitmap(),
                            contentDescription = loaded.name,
                            contentScale = ContentScale.Fit,
                            modifier = Modifier.fillMaxWidth(),
                        )
                        Text(
                            loaded.name,
                            style = MaterialTheme.typography.labelSmall,
                            color = ZeronColors.textMuted,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun PartView(part: Part, streaming: Boolean) {
    when (part) {
        is Part.Text -> MarkdownText(part.text)
        is Part.Reasoning -> ReasoningView(part, streaming)
        is Part.Tool -> Unit // tool calls are not conversation — see TranscriptView
        is Part.Input -> NoticeCard(
            // The panel carries the live question; the transcript keeps a
            // record of what was asked (first question, as a summary).
            text = part.questions.firstOrNull()?.question ?: stringResource(R.string.session_question),
            tone = ZeronColors.warning,
            background = ZeronColors.surfaceRaised,
        )
        is Part.Error -> NoticeCard(
            text = part.message,
            tone = ZeronColors.danger,
            background = ZeronColors.surface,
        )
    }
}

/**
 * Body text inside SelectionContainer (iOS isTextSelectionEnabled): the user
 * long-presses, marks any fragment and copies it from the native toolbar.
 * The whole-message copy lives in [CopyButton].
 */
@Composable
private fun SelectableText(
    text: AnnotatedString,
    color: androidx.compose.ui.graphics.Color,
    modifier: Modifier = Modifier,
) {
    SelectionContainer(modifier = modifier) {
        Text(
            text,
            style = MaterialTheme.typography.bodyLarge,
            color = color,
        )
    }
}

/**
 * Copy the whole message to the clipboard, then toast. Text parts only —
 * reasoning folds and tool calls are bookkeeping, not the conversation.
 * Selection (SelectionContainer below) covers any fragment.
 */
@Composable
private fun CopyButton(plainText: String) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val copiedLabel = stringResource(R.string.copy_copied)
    IconButton(
        onClick = {
            scope.launch {
                clipboard.setClipEntry(ClipData.newPlainText("text", plainText).toClipEntry())
            }
            android.widget.Toast.makeText(context, copiedLabel, android.widget.Toast.LENGTH_SHORT).show()
        },
        modifier = Modifier.size(26.dp),
    ) {
        Icon(
            painterResource(R.drawable.ic_copy),
            contentDescription = stringResource(R.string.copy_message),
            tint = ZeronColors.textFaint,
            modifier = Modifier.size(14.dp),
        )
    }
}

/** Rendered Markdown — headings, bullets, fenced code and inline spans. */
@Composable
fun MarkdownText(source: String, color: androidx.compose.ui.graphics.Color = ZeronColors.text) {
    val blocks = remember(source) { Markdown.parse(source) }
    Column(verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm)) {
        blocks.forEach { block ->
            when (block) {
                is MdBlock.Paragraph -> SelectableText(block.text, color)
                is MdBlock.Heading -> Text(
                    block.text,
                    style = when (block.level) {
                        1 -> MaterialTheme.typography.titleMedium.copy(fontSize = 18.sp)
                        2 -> MaterialTheme.typography.titleMedium
                        else -> MaterialTheme.typography.titleSmall
                    },
                    color = color,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(top = ZeronSpacing.xs),
                )
                is MdBlock.Bullet -> Column(
                    verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
                ) {
                    block.items.forEach { item ->
                        Row {
                            Text(
                                "•",
                                style = MaterialTheme.typography.bodyLarge,
                                color = ZeronColors.textMuted,
                            )
                            SelectableText(
                                item,
                                color = color,
                                modifier = Modifier.padding(start = ZeronSpacing.sm),
                            )
                        }
                    }
                }
                is MdBlock.Code -> CodeBlock(block.code, block.lang)
            }
        }
    }
}

@Composable
fun CodeBlock(code: String, lang: String?) {
    Column(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surface)
            .border(1.dp, ZeronColors.border, MaterialTheme.shapes.medium)
            .padding(ZeronSpacing.md),
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        if (!lang.isNullOrBlank()) {
            Text(
                lang,
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
            )
        }
        // Code must not reflow: wrapping a shell line changes what it says.
        // SelectionContainer so a long-press grabs the command, brackets and all.
        SelectionContainer {
            Text(
                code,
                style = MonoStyle,
                color = ZeronColors.text,
                softWrap = false,
                modifier = Modifier.horizontalScroll(rememberScrollState()),
            )
        }
    }
}

/**
 * A thought, folded away. Collapsed it is one muted line; while the turn is
 * still streaming this part it opens itself so the reader sees the model
 * thinking, and a manual tap then wins over that for the rest of the session.
 */
@Composable
private fun ReasoningView(part: Part.Reasoning, streaming: Boolean) {
    var overridden by rememberSaveable(part.id) { mutableStateOf(false) }
    var manual by rememberSaveable(part.id) { mutableStateOf(false) }
    val expanded = if (overridden) manual else streaming
    val chevronRotation by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "reasoningChevron",
    )
    Column(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surface)
            .border(1.dp, ZeronColors.border, MaterialTheme.shapes.medium)
            .clickable {
                manual = !expanded
                overridden = true
            }
            .padding(ZeronSpacing.md),
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                stringResource(R.string.session_thinking).uppercase(),
                style = MaterialTheme.typography.labelSmall,
                color = ZeronColors.textFaint,
            )
            Box(Modifier.weight(1f))
            Icon(
                painterResource(R.drawable.ic_expand_more),
                contentDescription = stringResource(
                    if (expanded) R.string.session_details_hide else R.string.session_details_show
                ),
                tint = ZeronColors.textFaint,
                modifier = Modifier.size(18.dp).rotate(chevronRotation),
            )
        }
        Text(
            // Collapsed: the opening line only, so the fold still says what
            // the model was chewing on.
            if (expanded) part.text else part.text.lineSequence().firstOrNull { it.isNotBlank() }.orEmpty(),
            style = MaterialTheme.typography.bodyMedium,
            color = ZeronColors.textMuted,
            maxLines = if (expanded) Int.MAX_VALUE else 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun NoticeCard(
    text: String,
    tone: androidx.compose.ui.graphics.Color,
    background: androidx.compose.ui.graphics.Color,
) {
    Text(
        text,
        style = MaterialTheme.typography.bodyLarge,
        color = tone,
        modifier = Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(background)
            .padding(ZeronSpacing.md),
    )
}
