package sh.zeron.android.testing

import sh.zeron.android.config.AppConfig
import sh.zeron.android.config.AuthMode

object Fakes {
    fun appConfig(deviceId: String = "test-device") = AppConfig(
        edgeBaseUrl = "https://edge.test",
        authMode = AuthMode.Dev,
        deviceId = deviceId,
    )
}
