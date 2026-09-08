package sh.zeron.android.ui.workspace

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import sh.zeron.android.R
import sh.zeron.android.data.ChatRow
import sh.zeron.android.sync.SendState
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun WorkspaceScreen(
    chats: List<ChatRow>,
    spaces: List<sh.zeron.android.data.SpaceRow> = emptyList(),
    connected: Boolean,
    error: String?,
    onOpen: (String) -> Unit,
    onNewSession: () -> Unit,
    onNewSpace: () -> Unit = {},
    onRetry: () -> Unit,
    onSignOut: () -> Unit,
    /** Delivery badges per chat (iOS HomeView sendBadge: Failed > Queued). */
    badges: Map<String, SendState?> = emptyMap(),
) {
    // iOS HomeView "+": no spaces yet → create one first; otherwise start a
    // session (the Android picker chooses the space).
    val primaryAction = if (spaces.isEmpty()) onNewSpace else onNewSession
    Scaffold(
        containerColor = ZeronColors.bg,
        topBar = {
            Column {
                TopAppBar(
                    title = {
                        Text(
                            stringResource(R.string.workspace_title),
                            style = MaterialTheme.typography.titleMedium,
                            color = ZeronColors.text,
                        )
                    },
                    actions = {
                        // iOS HomeView "+": no spaces yet → new space; else session.
                        IconButton(onClick = primaryAction) {
                            Icon(
                                painterResource(R.drawable.ic_add),
                                contentDescription = stringResource(R.string.workspace_new_session),
                                tint = ZeronColors.text,
                            )
                        }
                        OverflowMenu(onSignOut)
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = ZeronColors.surface,
                        titleContentColor = ZeronColors.text,
                    ),
                )
                HorizontalDivider(color = ZeronColors.border)
            }
        },
    ) { padding ->
        when {
            error != null -> StatusPane(
                padding = padding,
                icon = R.drawable.ic_error_outline,
                tint = ZeronColors.danger,
                title = stringResource(R.string.workspace_error_title),
                body = error,
                actionLabel = stringResource(R.string.workspace_retry),
                onAction = onRetry,
            )

            chats.isEmpty() && !connected -> StatusPane(
                padding = padding,
                icon = R.drawable.ic_cloud_off,
                tint = ZeronColors.textMuted,
                title = stringResource(R.string.workspace_offline_title),
                body = stringResource(R.string.workspace_offline_body),
                actionLabel = stringResource(R.string.workspace_retry),
                onAction = onRetry,
            )

            chats.isEmpty() -> StatusPane(
                padding = padding,
                icon = R.drawable.ic_forum,
                tint = ZeronColors.textFaint,
                title = stringResource(R.string.workspace_empty_title),
                body = stringResource(R.string.workspace_empty_body),
            )

            else -> SessionList(chats, padding, onOpen, badges)
        }
    }
}

@Composable
private fun OverflowMenu(onSignOut: () -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(
                painterResource(R.drawable.ic_more_vert),
                contentDescription = stringResource(R.string.workspace_more_actions),
                tint = ZeronColors.textMuted,
            )
        }
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            containerColor = ZeronColors.surfaceRaised,
        ) {
            DropdownMenuItem(
                text = {
                    Text(
                        stringResource(R.string.workspace_sign_out),
                        color = ZeronColors.text,
                    )
                },
                leadingIcon = {
                    Icon(
                        painterResource(R.drawable.ic_logout),
                        contentDescription = null,
                        tint = ZeronColors.textMuted,
                    )
                },
                onClick = { open = false; onSignOut() },
            )
        }
    }
}

@Composable
private fun SessionList(
    chats: List<ChatRow>,
    padding: PaddingValues,
    onOpen: (String) -> Unit,
    badges: Map<String, SendState?> = emptyMap(),
) {
    val (active, archived) = remember(chats) { chats.partition { !it.archived } }
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
        if (active.isNotEmpty()) {
            item(key = "header:active") { SectionLabel(stringResource(R.string.workspace_section_active)) }
            items(active, key = { it.id }) { SessionRow(it, onOpen, badges[it.id]) }
        }
        if (archived.isNotEmpty()) {
            item(key = "header:archived") { SectionLabel(stringResource(R.string.workspace_section_archived)) }
            items(archived, key = { it.id }) { SessionRow(it, onOpen, badges[it.id]) }
        }
    }
}

@Composable
private fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelSmall,
        color = ZeronColors.textFaint,
        modifier = Modifier.padding(top = ZeronSpacing.sm, bottom = ZeronSpacing.xs),
    )
}

@Composable
private fun SessionRow(chat: ChatRow, onOpen: (String) -> Unit, badge: SendState? = null) {
    val label = chat.title?.takeIf { it.isNotBlank() } ?: chat.id
    val description = stringResource(R.string.workspace_open_session, label)
    Row(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surface)
            .clickable { onOpen(chat.id) }
            .semantics { contentDescription = description }
            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.md),
    ) {
        Box(
            Modifier
                .size(7.dp)
                .clip(CircleShape)
                .background(if (chat.archived) ZeronColors.textFaint else ZeronColors.completed)
        )
        Column(Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    label,
                    style = MaterialTheme.typography.bodyLarge,
                    color = ZeronColors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                // iOS HomeView sendBadge (shell.rs precedence: Failed > Queued):
                // a muted dot + word in the row's status corner, no badge for
                // healthy in-flight sends.
                if (badge != null) SendBadge(badge)
            }
            if (chat.archived) {
                Text(
                    stringResource(R.string.workspace_archived_label),
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.textFaint,
                )
            }
        }
        Icon(
            painterResource(R.drawable.ic_chevron_right),
            contentDescription = null,
            tint = ZeronColors.textFaint,
            modifier = Modifier.size(18.dp),
        )
    }
}

@Composable
private fun SendBadge(state: SendState) {
    val (label, color) = when (state) {
        SendState.Failed -> stringResource(R.string.workspace_send_failed) to ZeronColors.danger
        SendState.Queued -> stringResource(R.string.workspace_send_queued) to ZeronColors.warning
        SendState.Sending -> return // healthy in-flight: no badge (iOS parity)
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        modifier = Modifier.padding(start = ZeronSpacing.sm),
    ) {
        Box(Modifier.size(6.dp).clip(CircleShape).background(color))
        Text(
            label,
            style = MaterialTheme.typography.labelSmall,
            color = color,
            fontWeight = androidx.compose.ui.text.font.FontWeight.Medium,
        )
    }
}

/**
 * Empty, disconnected and error all share one shape: mark, title, one line of
 * explanation, and an action when there is something the user can actually do.
 * They used to be a single centred line of body text with no way forward.
 */
@Composable
private fun StatusPane(
    padding: PaddingValues,
    icon: Int,
    tint: androidx.compose.ui.graphics.Color,
    title: String,
    body: String,
    actionLabel: String? = null,
    onAction: (() -> Unit)? = null,
) {
    Box(
        Modifier.fillMaxSize().padding(padding),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier.padding(horizontal = ZeronSpacing.xxl),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
        ) {
            Icon(
                painterResource(icon),
                contentDescription = null,
                tint = tint,
                modifier = Modifier.size(40.dp),
            )
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = ZeronSpacing.xs),
            )
            Text(
                body,
                style = MaterialTheme.typography.bodyMedium,
                color = ZeronColors.textMuted,
                textAlign = TextAlign.Center,
            )
            if (actionLabel != null && onAction != null) {
                Button(
                    onClick = onAction,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = ZeronColors.surfaceRaised,
                        contentColor = ZeronColors.text,
                    ),
                    modifier = Modifier.padding(top = ZeronSpacing.md),
                ) {
                    Icon(
                        painterResource(R.drawable.ic_refresh),
                        contentDescription = null,
                        modifier = Modifier.size(18.dp),
                    )
                    Text(
                        actionLabel,
                        modifier = Modifier.padding(start = ZeronSpacing.sm),
                    )
                }
            }
        }
    }
}
