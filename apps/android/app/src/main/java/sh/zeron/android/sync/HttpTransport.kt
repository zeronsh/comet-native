package sh.zeron.android.sync

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.Call
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import sh.zeron.android.core.AppError
import java.util.concurrent.TimeUnit

interface HttpTransport {
    suspend fun get(url: String, headers: Map<String, String> = emptyMap()): HttpResponse
    suspend fun post(url: String, body: ByteArray, headers: Map<String, String> = emptyMap()): HttpResponse
}

data class HttpResponse(val code: Int, val body: ByteArray, val headers: Map<String, String> = emptyMap()) {
    fun text(): String = String(body, Charsets.UTF_8)
}

class OkHttpTransport(
    private val client: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(30, TimeUnit.SECONDS)
        .callTimeout(60, TimeUnit.SECONDS)
        .build(),
) : HttpTransport {
    override suspend fun get(url: String, headers: Map<String, String>): HttpResponse =
        execute(Request.Builder().url(url).apply { headers.forEach { (k, v) -> header(k, v) } }.get().build())

    override suspend fun post(url: String, body: ByteArray, headers: Map<String, String>): HttpResponse {
        val reqBody = body.toRequestBody("application/json; charset=utf-8".toMediaType(), 0, body.size)
        return execute(Request.Builder().url(url).apply { headers.forEach { (k, v) -> header(k, v) } }.post(reqBody).build())
    }

    private suspend fun execute(request: Request): HttpResponse = withContext(Dispatchers.IO) {
        try {
            client.newCall(request).execute().use { resp -> HttpResponse(resp.code, resp.body?.bytes() ?: ByteArray(0)) }
        } catch (e: Exception) {
            throw AppError.Transport(e.message?.toIntOrNull())
        }
    }
}

class FakeHttpTransport(
    var handler: suspend (String, ByteArray, Map<String, String>) -> HttpResponse =
        { _, _, _ -> HttpResponse(200, ByteArray(0)) },
) : HttpTransport {
    override suspend fun get(url: String, headers: Map<String, String>) = handler(url, ByteArray(0), headers)
    override suspend fun post(url: String, body: ByteArray, headers: Map<String, String>) = handler(url, body, headers)
}