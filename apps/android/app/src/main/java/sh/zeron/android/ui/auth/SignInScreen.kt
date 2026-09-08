package sh.zeron.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import sh.zeron.android.R
import sh.zeron.android.ui.theme.ZeronColors
import sh.zeron.android.ui.theme.ZeronSpacing

/**
 * WorkOS AuthKit sign-in — one button that opens the hosted login in a browser
 * tab and returns through `zeron://callback` (same flow as iOS SignInView:
 * the mark, the tagline, one white button).
 */
@Composable
fun SignInScreen(onLogIn: () -> Unit, isLoading: Boolean = false) {
    Box(Modifier.fillMaxSize().background(ZeronColors.bg)) {
        Column(
            Modifier
                .fillMaxSize()
                .statusBarsPadding()
                .padding(horizontal = ZeronSpacing.xxl, vertical = ZeronSpacing.xxl),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box(
                Modifier.size(64.dp).clip(MaterialTheme.shapes.large).background(ZeronColors.surfaceRaised),
                contentAlignment = Alignment.Center,
            ) {
                Text("Z", style = MaterialTheme.typography.headlineMedium, color = ZeronColors.text)
            }
            Text(
                "Zeron",
                style = MaterialTheme.typography.headlineMedium,
                color = ZeronColors.text,
                modifier = Modifier.padding(top = ZeronSpacing.xl),
            )
            Text(
                stringResource(R.string.signin_tagline),
                style = MaterialTheme.typography.bodyMedium,
                color = ZeronColors.textMuted,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = ZeronSpacing.xs),
            )
            Button(
                onClick = onLogIn,
                enabled = !isLoading,
                shape = MaterialTheme.shapes.medium,
                colors = ButtonDefaults.buttonColors(
                    containerColor = ZeronColors.text,
                    contentColor = ZeronColors.bg,
                    disabledContainerColor = ZeronColors.surfaceRaised,
                    disabledContentColor = ZeronColors.textFaint,
                ),
                modifier = Modifier
                    .padding(top = ZeronSpacing.xxl)
                    .fillMaxWidth()
                    .semantics { contentDescription = "Log in to Zeron" },
            ) {
                if (isLoading) {
                    CircularProgressIndicator(Modifier.size(20.dp), color = ZeronColors.textMuted, strokeWidth = 2.dp)
                } else {
                    Text(stringResource(R.string.signin_action), style = MaterialTheme.typography.titleMedium)
                }
            }
        }
    }
}
