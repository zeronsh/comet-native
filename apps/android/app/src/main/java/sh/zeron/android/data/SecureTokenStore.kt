package sh.zeron.android.data

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

class SecureTokenStore(context: Context) : TokenStore {
    private val prefs by lazy {
        val key = MasterKey.Builder(context).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build()
        EncryptedSharedPreferences.create(
            context, "zeron_tokens", key,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM
        )
    }
    override suspend fun save(access: String, refresh: String, orgId: String?) {
        prefs.edit().putString("access", access).putString("refresh", refresh)
            .apply { if (orgId != null) putString("org", orgId) else remove("org") }
            .apply()
    }
    override suspend fun load(): Pair<String, String>? {
        val a = prefs.getString("access", null) ?: return null
        val r = prefs.getString("refresh", null) ?: return null
        return a to r
    }
    override suspend fun orgId(): String? = prefs.getString("org", null)
    override suspend fun clear() { prefs.edit().clear().apply() }
}
