package sh.zeron.android.data

import android.content.SharedPreferences
import java.util.UUID

interface DeviceIdStore {
    suspend fun getOrCreate(): String
    suspend fun reset()
}

class InMemoryDeviceIdStore : DeviceIdStore {
    @Volatile private var id: String? = null
    private val lock = Any()
    override suspend fun getOrCreate(): String = synchronized(lock) {
        id ?: UUID.randomUUID().toString().lowercase().also { id = it }
    }
    override suspend fun reset() { synchronized(lock) { id = null } }
}

class PersistentDeviceIdStore(private val prefs: SharedPreferences) : DeviceIdStore {
    override suspend fun getOrCreate(): String {
        val existing = prefs.getString(KEY, null)
        if (existing != null) return existing
        val id = UUID.randomUUID().toString().lowercase()
        prefs.edit().putString(KEY, id).apply()
        return id
    }
    override suspend fun reset() { prefs.edit().remove(KEY).apply() }
    private companion object { const val KEY = "device_id" }
}