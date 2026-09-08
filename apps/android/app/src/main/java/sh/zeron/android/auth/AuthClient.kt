package sh.zeron.android.auth

import org.json.JSONObject
import sh.zeron.android.config.AppConfig
import sh.zeron.android.core.AppError
import sh.zeron.android.sync.HttpTransport

data class AuthUser(val id: String, val email: String?)
data class AuthOrg(val id: String, val organizationId: String, val name: String)
data class AuthTokens(val accessToken: String, val refreshToken: String)

interface AuthClient {
    suspend fun exchange(code: String): Pair<AuthUser, AuthTokens>
    suspend fun refresh(refreshToken: String, organizationId: String? = null): AuthTokens
    suspend fun orgs(accessToken: String): List<AuthOrg>
}

class HttpAuthClient(
    private val config: AppConfig,
    private val http: HttpTransport,
) : AuthClient {
    private fun url(path: String) = "${config.edgeBaseUrl.trimEnd('/')}/$path"

    override suspend fun exchange(code: String): Pair<AuthUser, AuthTokens> {
        val body = JSONObject().put("code", code).toString().toByteArray()
        val resp = http.post(url("auth/exchange"), body)
        checkStatus(resp)
        val o = JSONObject(resp.text())
        val user = o.getJSONObject("user")
        val tokens = AuthTokens(o.getString("accessToken"), o.getString("refreshToken"))
        return AuthUser(user.getString("id"), user.optString("email").ifBlank { null }) to tokens
    }

    override suspend fun refresh(refreshToken: String, organizationId: String?): AuthTokens {
        val body = JSONObject().apply {
            put("refreshToken", refreshToken)
            if (organizationId != null) put("organizationId", organizationId)
        }.toString().toByteArray()
        val resp = http.post(url("auth/refresh"), body)
        checkStatus(resp)
        val o = JSONObject(resp.text())
        return AuthTokens(o.getString("accessToken"), o.getString("refreshToken"))
    }

    override suspend fun orgs(accessToken: String): List<AuthOrg> {
        val resp = http.get(url("auth/orgs"), mapOf("Authorization" to "Bearer $accessToken"))
        checkStatus(resp)
        val arr = JSONObject(resp.text()).getJSONArray("orgs")
        return (0 until arr.length()).map { i ->
            val o = arr.getJSONObject(i)
            AuthOrg(o.getString("id"), o.getString("organizationId"), o.getString("name"))
        }
    }

    private fun checkStatus(resp: sh.zeron.android.sync.HttpResponse) {
        if (resp.code !in 200..299) {
            throw AppError.Auth(resp.code, resp.text().take(200))
        }
    }
}

class FakeAuthClient : AuthClient {
    var tokens = AuthTokens("fake-access", "fake-refresh")
    var orgs = listOf(AuthOrg("1", "org-1", "Org One"))
    override suspend fun exchange(code: String) = AuthUser("u1", "u@ex.com") to tokens
    override suspend fun refresh(refreshToken: String, organizationId: String?) = tokens
    override suspend fun orgs(accessToken: String) = orgs
}