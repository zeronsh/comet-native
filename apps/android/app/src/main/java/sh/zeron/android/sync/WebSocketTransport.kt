package sh.zeron.android.sync

import kotlinx.coroutines.flow.Flow

interface WebSocketTransport {
    fun connect(url: String): Flow<WsMessage>
    suspend fun send(message: WsMessage)
    suspend fun close()
}

sealed class WsMessage {
    data class Text(val text: String) : WsMessage()
    data class Binary(val bytes: ByteArray) : WsMessage()
    object Connected : WsMessage()
    object Closed : WsMessage()
}

open class FakeWebSocketTransport : WebSocketTransport {
    private val inbound = kotlinx.coroutines.flow.MutableSharedFlow<WsMessage>()
    override fun connect(url: String): Flow<WsMessage> = inbound
    override suspend fun send(message: WsMessage) {}
    override suspend fun close() {}
    suspend fun emit(msg: WsMessage) = inbound.emit(msg)
}
