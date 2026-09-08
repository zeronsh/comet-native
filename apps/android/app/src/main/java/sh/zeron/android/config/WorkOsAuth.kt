package sh.zeron.android.config

/**
 * WorkOS AuthKit endpoints — mirrors iOS `Endpoints`
 * (apps/ios/Zeron/Views/SignInView.swift:12). Fixed to production: mobile
 * always talks to prod, a stale override once broke sign-in.
 */
object WorkOsAuth {
    const val CLIENT_ID = "client_01KWD0EAKZKD50YCQJNYSRE4BY"
    const val API_BASE = "https://api.workos.com"
    const val CALLBACK_SCHEME = "zeron"
    const val CALLBACK_HOST = "callback"
    const val REDIRECT_URI = "$CALLBACK_SCHEME://$CALLBACK_HOST"

    /** The authorization-code URL opened in a Custom Tab. */
    fun authorizeUrl(state: String): String = buildString {
        append("$API_BASE/user_management/authorize")
        append("?response_type=code")
        append("&client_id=$CLIENT_ID")
        append("&redirect_uri=").append(java.net.URLEncoder.encode(REDIRECT_URI, "UTF-8"))
        append("&provider=authkit")
        append("&state=").append(java.net.URLEncoder.encode(state, "UTF-8"))
    }
}
