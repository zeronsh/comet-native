package sh.zeron.android.auth

import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import sh.zeron.android.data.TokenStore

class AuthStateMachine(
    private val client: AuthClient,
    private val tokens: TokenStore,
) {
    private val refreshMutex = Mutex()
    var selectedOrgId: String? = null

    /** WorkOS paste-code exchange (iOS AppModel.signIn). */
    suspend fun signInWithCode(code: String): List<AuthOrg> = refreshMutex.withLock {
        val (_, t) = client.exchange(code)
        tokens.save(t.accessToken, t.refreshToken)
        client.orgs(t.accessToken)
    }

    /**
     * iOS AppModel.restore parity: load the persisted pair + the org the
     * tokens were scoped to. null = no persisted session (show sign-in).
     */
    suspend fun restoreSession(): String? = refreshMutex.withLock {
        if (tokens.load() == null) return null
        val org = tokens.orgId()
        selectedOrgId = org
        org
    }

    /** Current access token for socket URLs (never logged). */
    suspend fun accessToken(): String? = tokens.load()?.first

    /**
     * Org list for the stored access token — migration path for sessions
     * persisted before orgId was stored (re-derive the lost org scope).
     */
    suspend fun listOrgs(): List<AuthOrg> = refreshMutex.withLock {
        val access = tokens.load()?.first ?: return emptyList()
        client.orgs(access)
    }

    suspend fun selectOrgAndRefresh(orgId: String): AuthTokens = refreshMutex.withLock {
        val pair = tokens.load() ?: error("no refresh token")
        val scoped = client.refresh(pair.second, orgId)
        tokens.save(scoped.accessToken, scoped.refreshToken, orgId)
        selectedOrgId = orgId
        scoped
    }

    /** Dev mode (AUTH_MODE=dev edge): bearer IS the identity, no exchange. */
    suspend fun signInDev(userId: String, orgId: String): AuthTokens = refreshMutex.withLock {
        val bearer = "$userId@$orgId"
        val t = AuthTokens(bearer, bearer)
        tokens.save(t.accessToken, t.refreshToken, orgId)
        selectedOrgId = orgId
        t
    }

    suspend fun signOut() = refreshMutex.withLock {
        tokens.clear()
        selectedOrgId = null
    }

    suspend fun refreshSerialized(): AuthTokens = refreshMutex.withLock {
        val pair = tokens.load() ?: error("no refresh token")
        val next = client.refresh(pair.second, selectedOrgId)
        // Keep the org scope through rotation, or the next restore loses it.
        tokens.save(next.accessToken, next.refreshToken, selectedOrgId)
        next
    }
}
