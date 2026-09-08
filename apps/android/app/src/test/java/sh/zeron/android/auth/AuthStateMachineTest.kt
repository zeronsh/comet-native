package sh.zeron.android.auth

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import sh.zeron.android.data.InMemoryTokenStore

class AuthStateMachineTest {
    private class RecordingClient : AuthClient {
        var refreshedOrg: String? = "unset"
        override suspend fun exchange(code: String) = AuthUser("u1", null) to AuthTokens("at", "rt")
        override suspend fun refresh(refreshToken: String, organizationId: String?): AuthTokens {
            refreshedOrg = organizationId
            return AuthTokens("at2", "rt2")
        }
        override suspend fun orgs(accessToken: String) = emptyList<AuthOrg>()
    }

    @Test
    fun restoreReturnsPersistedOrgAndRefreshUsesIt() = runTest {
        val tokens = InMemoryTokenStore()
        val client = RecordingClient()
        val auth = AuthStateMachine(client, tokens)
        tokens.save("at", "rt", "org-9")

        assertEquals("org-9", auth.restoreSession())
        assertEquals("at", auth.accessToken())

        // A serialized refresh after restore is org-scoped (the restored org).
        auth.refreshSerialized()
        assertEquals("org-9", client.refreshedOrg)
    }

    @Test
    fun restoreWithNoTokensReturnsNull() = runTest {
        val auth = AuthStateMachine(RecordingClient(), InMemoryTokenStore())
        assertNull(auth.restoreSession())
    }

    @Test
    fun signOutClearsOrgSoRestoreReturnsNull() = runTest {
        val tokens = InMemoryTokenStore()
        val auth = AuthStateMachine(RecordingClient(), tokens)
        tokens.save("at", "rt", "org-9")
        auth.signOut()

        assertNull(auth.restoreSession())
    }
}
