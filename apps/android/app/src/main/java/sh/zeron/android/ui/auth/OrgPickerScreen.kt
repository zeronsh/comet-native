package sh.zeron.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.res.stringResource
import sh.zeron.android.R
import sh.zeron.android.auth.AuthOrg
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

@Composable
fun OrgPickerScreen(orgs: List<AuthOrg>, onSelect: (AuthOrg) -> Unit) {
    Box(Modifier.fillMaxSize().background(ZeronColors.bg)) {
        Column(
            Modifier
                .fillMaxSize()
                .statusBarsPadding()
                .padding(ZeronSpacing.xl),
            verticalArrangement = Arrangement.spacedBy(ZeronSpacing.sm),
        ) {
            Text(
                stringResource(R.string.org_picker_title),
                style = MaterialTheme.typography.headlineMedium,
                color = ZeronColors.text,
            )
            Text(
                stringResource(R.string.org_picker_subtitle),
                style = MaterialTheme.typography.bodyMedium,
                color = ZeronColors.textMuted,
            )
            Spacer(Modifier.height(ZeronSpacing.md))
            orgs.forEach { org ->
                Row(org, onSelect)
            }
        }
    }
}

@Composable
private fun Row(org: AuthOrg, onSelect: (AuthOrg) -> Unit) {
    val description = stringResource(R.string.org_picker_select, org.name)
    Column(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(ZeronColors.surface)
            .clickable { onSelect(org) }
            .padding(horizontal = ZeronSpacing.lg, vertical = ZeronSpacing.md)
            .semantics { contentDescription = description },
    ) {
        Text(org.name, style = MaterialTheme.typography.bodyLarge, color = ZeronColors.text)
        Text(org.organizationId, style = MaterialTheme.typography.labelSmall, color = ZeronColors.textFaint)
    }
}
