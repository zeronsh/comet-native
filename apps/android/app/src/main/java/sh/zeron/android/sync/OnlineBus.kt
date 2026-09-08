package sh.zeron.android.sync

import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull

/**
 * The online/wake bus — Android port of iOS OnlineBus (crates/sync/src/wake.rs,
 * PR #168's event-driven recovery): \"the network is back\" becomes an event
 * every parked backoff can select on, so recovery is event-driven, never timer
 * luck. Waiters park on [waitBackoff] and wake on the first online event
 * during their wait; every successful room join and the OS offline→online
 * transition broadcast.
 */
object OnlineBus {
    /** While the OS path is offline, backoff waits park at least this long (wake.rs). */
    const val OFFLINE_PARK_RECHECK_MS = 30_000L

    private val onlineEvents = MutableSharedFlow<Unit>(extraBufferCapacity = 1)

    /** Empirical "the network is back": resume every parked waiter. */
    fun notifyOnline() {
        onlineEvents.tryEmit(Unit)
    }

    /**
     * Sleep `ms`, cut short by an online event that fires during the wait (iOS
     * OnlineBus.waitBackoff). While the OS path reports offline the wait is
     * stretched to at least the 30s park recheck — dials against a dead path
     * are pure battery.
     */
    suspend fun waitBackoff(ms: Long) {
        val wait = if (Connectivity.offline.value) maxOf(ms, OFFLINE_PARK_RECHECK_MS) else ms
        withTimeoutOrNull(wait) { onlineEvents.first() }
    }
}
