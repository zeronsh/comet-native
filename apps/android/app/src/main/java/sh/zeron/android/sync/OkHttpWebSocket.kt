package sh.zeron.android.sync

import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.concurrent.TimeUnit

class OkHttpWebSocket(
    private val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .pingInterval(15, TimeUnit.SECONDS)
        .build(),
) : WebSocketTransport {
    @Volatile private var socket: WebSocket? = null

    override fun connect(url: String): Flow<WsMessage> = callbackFlow {
        val request = Request.Builder().url(url).build()
        val listener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                trySend(WsMessage.Connected)
            }
            override fun onMessage(webSocket: WebSocket, text: String) {
                trySend(WsMessage.Text(text))
            }
            override fun onMessage(webSocket: WebSocket, bytes: okio.ByteString) {
                trySend(WsMessage.Binary(bytes.toByteArray()))
            }
            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(code, reason)
            }
            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                socket = null
                trySend(WsMessage.Closed)
            }
            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                socket = null
                trySend(WsMessage.Closed)
            }
        }
        val ws = client.newWebSocket(request, listener)
        socket = ws
        awaitClose { ws.close(1000, "bye") }
    }

    override suspend fun send(message: WsMessage) {
        when (message) {
            is WsMessage.Text -> socket?.send(message.text)
            is WsMessage.Binary -> socket?.send(okio.ByteString.of(*message.bytes))
            is WsMessage.Connected, is WsMessage.Closed -> {}
        }
    }

    override suspend fun close() { socket?.close(1000, "bye"); socket = null }
}

/** Registry JSON frames — mirror RegistryCodec; kept transport-independent. */
object RegistryJson {
    fun hello(cursor: Long?, device: String): String {
        val o = JSONObject().put("t", "hello").put("device", device)
        o.put("cursor", if (cursor == null) JSONObject.NULL else cursor)
        return o.toString()
    }
}