package sh.zeron.android.sync

import sh.zeron.android.auth.AuthOrg

sealed class AppState {
    object SignedOut : AppState()
    object SigningIn : AppState()
    data class SelectingOrg(val orgs: List<AuthOrg>) : AppState()
    object Connecting : AppState()
    object Ready : AppState()
    object Disconnected : AppState()
    data class Fatal(val message: String) : AppState()
}
