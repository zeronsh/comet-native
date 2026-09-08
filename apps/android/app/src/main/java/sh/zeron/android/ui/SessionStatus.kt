package sh.zeron.android.ui

import androidx.annotation.StringRes
import sh.zeron.android.R

/**
 * What the session header is allowed to say.
 *
 * The sync layer's raw strings ("ws_failed", "native doc failed: …", "history
 * compacted — older messages need checkpoint fetch") used to be rendered
 * verbatim in the app bar. They now land in [Failed.detail], which
 * the screen can reveal on demand, while the chip shows a short human label.
 */
sealed interface SessionStatus {
    data object Connecting : SessionStatus
    data object Connected : SessionStatus
    data object SignedOut : SessionStatus
    data object HistoryTrimmed : SessionStatus
    data class Failed(val detail: String) : SessionStatus

    @get:StringRes
    val labelRes: Int
        get() = when (this) {
            Connecting -> R.string.status_connecting
            Connected -> R.string.status_connected
            SignedOut -> R.string.status_signed_out
            HistoryTrimmed -> R.string.status_history_trimmed
            is Failed -> R.string.status_failed
        }

    /** Detail worth offering the user; null when the label already says it all. */
    val detailOrNull: String?
        get() = (this as? Failed)?.detail?.takeIf { it.isNotBlank() }
}
