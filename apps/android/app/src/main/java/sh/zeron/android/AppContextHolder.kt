package sh.zeron.android

/**
 * Application-context holder — disk-backed stores (DocDisk, UploadStash) need
 * a Context; ZeronApp.onCreate seeds it before any ViewModel is built.
 */
object AppContextHolder {
    @Volatile
    var context: android.content.Context? = null
        private set

    fun init(context: android.content.Context) {
        if (this.context == null) this.context = context.applicationContext
    }
}
