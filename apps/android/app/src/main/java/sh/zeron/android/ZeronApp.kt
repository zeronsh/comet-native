package sh.zeron.android

import android.app.Application

class ZeronApp : Application() {
    override fun onCreate() {
        super.onCreate()
        AppContextHolder.init(this)
    }
}
