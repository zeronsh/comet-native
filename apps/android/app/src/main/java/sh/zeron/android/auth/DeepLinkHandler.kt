package sh.zeron.android.auth

import android.net.Uri

data class AuthCallback(val code: String, val state: String)

object DeepLinkHandler {
    fun parse(uri: Uri, expectedState: String): AuthCallback? {
        val code = uri.getQueryParameter("code") ?: return null
        val state = uri.getQueryParameter("state") ?: return null
        if (state != expectedState) return null
        // Never log code
        return AuthCallback(code, state)
    }
}
