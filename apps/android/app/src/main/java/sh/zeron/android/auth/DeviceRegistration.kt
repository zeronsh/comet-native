package sh.zeron.android.auth

import sh.zeron.android.data.DeviceIdStore

class DeviceRegistration(private val deviceIds: DeviceIdStore) {
    suspend fun deviceId(): String = deviceIds.getOrCreate()
    fun displayName(): String = "Android"
}
