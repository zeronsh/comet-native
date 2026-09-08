package sh.zeron.android.data

interface TokenStore {
    suspend fun save(access: String, refresh: String, orgId: String? = null)
    suspend fun load(): Pair<String, String>?
    /** Persisted org the tokens were scoped to (needed to restore a session). */
    suspend fun orgId(): String?
    suspend fun clear()
}

class InMemoryTokenStore : TokenStore {
    @Volatile private var pair: Pair<String, String>? = null
    @Volatile private var org: String? = null
    override suspend fun save(access: String, refresh: String, orgId: String?) {
        pair = access to refresh
        org = orgId
    }
    override suspend fun load(): Pair<String, String>? = pair
    override suspend fun orgId(): String? = org
    override suspend fun clear() {
        pair = null
        org = null
    }
}
