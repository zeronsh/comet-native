package sh.zeron.android.config

data class AppConfig(
    val edgeBaseUrl: String,
    val authMode: AuthMode,
    val deepLinkScheme: String = "sh.zeron.auth",
    val deepLinkHost: String = "callback",
    val deviceId: String,
) {
    init {
        require(edgeBaseUrl.startsWith("https://") || authMode == AuthMode.Dev) {
            "production config must be https"
        }
        require(edgeBaseUrl.isNotBlank()) { "edgeBaseUrl required" }
        require(deviceId.isNotBlank()) { "deviceId required" }
    }

    val isDev: Boolean get() = authMode == AuthMode.Dev
}

enum class AuthMode { WorkOS, Dev }
