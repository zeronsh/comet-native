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
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
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
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import sh.zeron.android.R
import sh.zeron.android.data.DeviceRow
import sh.zeron.android.data.FolderEntry
import sh.zeron.android.data.FolderListing
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

/** A device is online while its last presence beat is this fresh (iOS presenceTtlMs). */
private const val NEW_SPACE_PRESENCE_TTL_MS = 30_000L

/**
 * New space (iOS NewSpaceSheet parity): pick the desktop device + folder the
 * space will own. Listing comes from the device over the relay (ListFolders);
 * dotfiles are pre-filtered and long listings are truncated at 500 by the
 * engine. "Use this folder" creates the space (Mutate to the host; local
 * upsert fallback).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewSpaceScreen(
    devices: List<DeviceRow>,
    presence: Map<String, Long>,
    listing: FolderListing?,
    loading: Boolean,
    error: String?,
    creating: Boolean,
    currentIsRepo: Boolean,
    folderDeviceId: String?,
    onPickDevice: (String) -> Unit,
    /** path + the isRepo flag of the folder navigated to (iOS currentIsRepo). */
    onNavigate: (String?, Boolean) -> Unit,
    onCreate: (String, String, Boolean) -> Unit,
    onBack: () -> Unit,
) {
    // Engines own folders; this phone can't (iOS filters platform != ios).
    val hosts = remember(devices) { devices.filter { it.platform != "ios" && it.platform != "android" } }
    val device = hosts.firstOrNull { it.id == (folderDeviceId ?: hosts.firstOrNull()?.id) }

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
                                contentDescription = stringResource(R.string.new_space_back),
                                tint = ZeronColors.text,
                            )
                        }
                    },
                    title = {
                        Text(
                            stringResource(R.string.new_space_title),
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
    ) { padding ->
        if (hosts.isEmpty()) {
            EmptyDevices(padding)
            return@Scaffold
        }
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            // Device tabs (horizontal pills, iOS deviceTabs).
            Row(
                Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
                horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
            ) {
                hosts.forEach { d ->
                    val selected = d.id == device?.id
                    Row(
                        Modifier
                            .clip(CircleShape)
                            .background(if (selected) ZeronColors.surfaceRaised else ZeronColors.surface)
                            .clickable {
                                if (d.id != device?.id) {
                                    onPickDevice(d.id)
                                }
                            }
                            .padding(horizontal = ZeronSpacing.md, vertical = ZeronSpacing.sm),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
                    ) {
                        Box(
                            Modifier
                                .size(6.dp)
                                .clip(CircleShape)
                                .background(
                                    if (deviceOnline(d.id, presence)) ZeronColors.completed
                                    else ZeronColors.textFaint
                                )
                        )
                        Text(
                            d.name,
                            style = MaterialTheme.typography.labelMedium,
                            color = if (selected) ZeronColors.text else ZeronColors.textMuted,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }
            // Breadcrumb bar: up button + current path.
            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.xs),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
            ) {
                IconButton(
                    onClick = { onNavigate(listing?.parent, false) },
                    enabled = listing?.parent != null,
                ) {
                    Icon(
                        painterResource(R.drawable.ic_arrow_back),
                        contentDescription = stringResource(R.string.new_space_up),
                        tint = if (listing?.parent != null) ZeronColors.text else ZeronColors.textFaint,
                        modifier = Modifier.size(18.dp),
                    )
                }
                Text(
                    listing?.path ?: " ",
                    style = MaterialTheme.typography.bodyMedium,
                    color = ZeronColors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (loading) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(14.dp),
                        color = ZeronColors.textFaint,
                        strokeWidth = 1.5.dp,
                    )
                }
            }
            FolderList(
                listing = listing,
                loading = loading,
                error = error,
                onOpen = { entry ->
                    val base = listing?.path ?: return@FolderList
                    val child = if (base.endsWith("/")) base + entry.name else "$base/${entry.name}"
                    onNavigate(child, entry.isRepo)
                },
            )
        }
    }
    // Pinned bottom "Use this folder" button (iOS safeAreaInset).
    if (hosts.isNotEmpty()) {
        val name = listing?.path?.substringAfterLast('/')?.takeIf { it.isNotEmpty() } ?: ""
        val enabled = listing != null && !creating && !loading
        Box(
            Modifier
                .fillMaxWidth()
                .background(ZeronColors.bg)
                .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.sm),
        ) {
            Button(
                onClick = {
                    val d = device ?: return@Button
                    val p = listing?.path ?: return@Button
                    onCreate(d.id, p, currentIsRepo)
                },
                enabled = enabled,
                colors = ButtonDefaults.buttonColors(
                    containerColor = ZeronColors.text,
                    contentColor = ZeronColors.bg,
                    disabledContainerColor = ZeronColors.surfaceRaised,
                    disabledContentColor = ZeronColors.textFaint,
                ),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(
                    if (creating) stringResource(R.string.new_space_creating)
                    else if (name.isEmpty()) stringResource(R.string.new_space_use_folder)
                    else stringResource(R.string.new_space_use_name, name),
                )
            }
        }
    }
}

@Composable
private fun EmptyDevices(padding: PaddingValues) {
    Box(
        Modifier.fillMaxSize().padding(padding),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier.padding(horizontal = ZeronSpacing.xxl),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
        ) {
            Text(
                stringResource(R.string.new_space_no_devices_title),
                style = MaterialTheme.typography.titleMedium,
                color = ZeronColors.text,
                textAlign = TextAlign.Center,
            )
            Text(
                stringResource(R.string.new_space_no_devices_body),
                style = MaterialTheme.typography.bodyMedium,
                color = ZeronColors.textMuted,
                textAlign = TextAlign.Center,
            )
        }
    }
}

@Composable
private fun FolderList(
    listing: FolderListing?,
    loading: Boolean,
    error: String?,
    onOpen: (FolderEntry) -> Unit,
) {
    val folders = remember(listing) { listing?.entries?.filter { it.isDir }.orEmpty() }
    LazyColumn(
        Modifier.fillMaxSize(),
        contentPadding = PaddingValues(
            start = ZeronSpacing.lg,
            end = ZeronSpacing.lg,
            bottom = ZeronSpacing.xl,
        ),
        verticalArrangement = Arrangement.spacedBy(ZeronSpacing.xs),
    ) {
        if (error != null) {
            item(key = "error") {
                Text(
                    error,
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.danger,
                    modifier = Modifier.padding(vertical = ZeronSpacing.sm),
                )
            }
        }
        if (folders.isEmpty() && !loading && error == null && listing != null) {
            item(key = "empty") {
                Text(
                    stringResource(R.string.new_space_no_folders),
                    style = MaterialTheme.typography.bodyMedium,
                    color = ZeronColors.textFaint,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.fillMaxWidth().padding(vertical = ZeronSpacing.xxl),
                )
            }
        }
        if (listing?.truncated == true) {
            item(key = "truncated") {
                Text(
                    stringResource(R.string.new_space_truncated),
                    style = MaterialTheme.typography.labelSmall,
                    color = ZeronColors.textFaint,
                    modifier = Modifier.padding(vertical = ZeronSpacing.xs),
                )
            }
        }
        items(folders, key = { it.name }) { entry ->
            Row(
                Modifier
                    .fillMaxWidth()
                    .clip(MaterialTheme.shapes.medium)
                    .background(ZeronColors.surface)
                    .clickable { onOpen(entry) }
                    .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ZeronSpacing.md),
            ) {
                Text(
                    entry.name,
                    style = MaterialTheme.typography.bodyLarge,
                    color = ZeronColors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (entry.isRepo) {
                    Text(
                        stringResource(R.string.new_space_git),
                        style = MaterialTheme.typography.labelSmall,
                        color = ZeronColors.accent,
                        modifier = Modifier
                            .clip(CircleShape)
                            .background(ZeronColors.accent.copy(alpha = 0.12f))
                            .padding(horizontal = ZeronSpacing.sm, vertical = 3.dp),
                    )
                }
                Icon(
                    painterResource(R.drawable.ic_chevron_right),
                    contentDescription = null,
                    tint = ZeronColors.textFaint,
                    modifier = Modifier.size(16.dp),
                )
            }
        }
    }
}

private fun deviceOnline(deviceId: String, presence: Map<String, Long>): Boolean {
    val seen = presence[deviceId] ?: return false
    return System.currentTimeMillis() - seen < NEW_SPACE_PRESENCE_TTL_MS
}
