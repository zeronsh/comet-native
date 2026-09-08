package sh.zeron.android.sync

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/**
 * A pending send's user-visible truth (iOS SendState / state.rs send_*).
 * `failed` wins over `queued` in the UI.
 */
enum class SendState {
    /** Healthy in-flight send (within the 2-minute grace). */
    Sending,
    /** Pending on a degraded path — will send automatically. */
    Queued,
    /** Unadopted past the grace — explicit tap-to-retry. */
    Failed,
}

/** state.rs UNDELIVERED_GRACE_MS: a send unadopted past this is Failed. */
const val UNDELIVERED_GRACE_MS: Long = 120_000

/**
 * OS-path connectivity truth — the phone half of iOS ConnectivityCenter
 * (`setPathOffline`), fed by MainActivity's ConnectivityManager callback. A
 * definitive offline path makes every send read `queued` (the doc still holds
 * the command; the room re-pushes when the path returns).
 */
object Connectivity {
    private val _offline = MutableStateFlow(false)
    val offline: StateFlow<Boolean> = _offline

    fun setPathOffline(offline: Boolean) {
        if (_offline.value != offline) _offline.value = offline
    }
}
