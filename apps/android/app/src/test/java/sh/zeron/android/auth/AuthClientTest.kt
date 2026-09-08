package sh.zeron.android.auth

import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import sh.zeron.android.config.AppConfig
import sh.zeron.android.config.AuthMode
import sh.zeron.android.core.AppError
import sh.zeron.android.sync.FakeHttpTransport
import sh.zeron.android.sync.HttpResponse

class AuthClientTest {
    private fun config() = AppConfig(edgeBaseUrl = "http://edge.test", authMode = AuthMode.Dev, deviceId = "d")

    @Test fun exchangeParsesUserAndTokens() = runTest {
        val http = FakeHttpTransport { url, body, _ ->
            assertTrue(url.endsWith("/auth/exchange"))
            assertEquals("abc", JSONObject(String(body)).getString("code"))
            HttpResponse(200, """{"user":{"id":"u1","email":"a@b.c"},"accessToken":"at","refreshToken":"rt"}""".toByteArray())
        }
        val (user, tokens) = HttpAuthClient(config(), http).exchange("abc")
        assertEquals("u1", user.id)
        assertEquals("at", tokens.accessToken)
        assertEquals("rt", tokens.refreshToken)
    }

    @Test fun refreshScopesByOrg() = runTest {
        val http = FakeHttpTransport { url, body, _ ->
            assertTrue(url.endsWith("/auth/refresh"))
            val o = JSONObject(String(body))
            assertEquals("rt", o.getString("refreshToken"))
            assertEquals("org-5", o.getString("organizationId"))
            HttpResponse(200, """{"accessToken":"at2","refreshToken":"rt2"}""".toByteArray())
        }
        val tokens = HttpAuthClient(config(), http).refresh("rt", "org-5")
        assertEquals("at2", tokens.accessToken)
    }

    @Test fun orgsSendsBearer() = runTest {
        var seen: String? = null
        val http = FakeHttpTransport { url, _, headers ->
            seen = headers["Authorization"]
            assertTrue(url.endsWith("/auth/orgs"))
            HttpResponse(200, """{"orgs":[{"id":"o","organizationId":"org-1","name":"Org"}]}""".toByteArray())
        }
        val orgs = HttpAuthClient(config(), http).orgs("tok")
        assertEquals("Bearer tok", seen)
        assertEquals(1, orgs.size)
        assertEquals("Org", orgs[0].name)
    }

    @Test fun non2xxThrowsAuthError() = runTest {
        val http = FakeHttpTransport { _, _, _ -> HttpResponse(401, """{"error":"bad"}""".toByteArray()) }
        val err = try { HttpAuthClient(config(), http).orgs("x"); null } catch (e: AppError.Auth) { e }
        assertEquals(401, err?.code)
    }
}