package sh.zeron.android.config

/**
 * Edge connection defaults. Mirrors iOS `Endpoints.edgeURL`
 * (apps/ios/Zeron/Views/SignInView.swift:13) — production edge, WorkOS AuthKit.
 * Dev mode (`user@org` bearer against an AUTH_MODE=dev edge) stays available
 * for debug builds but is never the default.
 */
object EdgeConfig {
    /** Same default as iOS. */
    const val PRODUCTION_EDGE = "https://edge.zeron.sh"

    val edgeBaseUrl: String = PRODUCTION_EDGE
    val authMode: AuthMode = AuthMode.WorkOS

    fun appConfig(deviceId: String) = AppConfig(
        edgeBaseUrl = edgeBaseUrl,
        authMode = authMode,
        deviceId = deviceId,
    )

    /** http→ws, https→wss (iOS AppConfig.wsBase). */
    private fun wsBase(base: String): String = when {
        base.startsWith("https://") -> "wss://" + base.removePrefix("https://")
        base.startsWith("http://") -> "ws://" + base.removePrefix("http://")
        else -> base
    }.trimEnd('/')

    /**
     * Registry room URL with the token+device query the DO requires
     * (iOS AppConfig.registrySocketURL).
     */
    fun registryWSUrl(orgId: String, token: String, deviceId: String): String =
        "${wsBase(edgeBaseUrl)}/registry/$orgId/ws?token=$token&device=$deviceId"

    /** chat2 room URL (iOS AppConfig.chat2SocketURL). */
    fun chat2WSUrl(chatId: String, token: String, deviceId: String): String =
        "${wsBase(edgeBaseUrl)}/chat2/$chatId/ws?token=$token&device=$deviceId"

    /**
     * Device-room relay URL for RPC to a host device (iOS DeviceRelayClient:
     * `/device/{deviceId}/ws?role=client&connId=…&token=…`). A fresh connId per
     * connect — reusing one can briefly leave two tagged sockets in the DO.
     */
    fun relayWsUrl(deviceId: String, token: String): String =
        "${wsBase(edgeBaseUrl)}/device/$deviceId/ws?role=client&connId=${java.util.UUID.randomUUID()}&token=$token"

    /**
     * GET /chat2/{chatId}/checkpoint — the Range-resumable doc snapshot a
     * fresh reader must fetch when the room's history was compacted (iOS
     * AppConfig.chat2CheckpointRequest). Auth rides ?token= like the socket
     * URLs (edge auth accepts both that and a Bearer header).
     */
    fun chat2CheckpointUrl(chatId: String, token: String): String =
        "${edgeBaseUrl.trimEnd('/')}/chat2/$chatId/checkpoint?token=$token"
}