package sh.zeron.android

object NativeLoader {
    @Volatile private var loaded = false

    fun loadOnce() {
        if (loaded) return
        synchronized(this) {
            if (loaded) return
            try {
                System.loadLibrary("zeron_loro_android")
                loaded = true
            } catch (e: UnsatisfiedLinkError) {
                // Single entry point; missing toolchain is surfaced clearly in logs.
                android.util.Log.w("ZeronNative", "native lib not present (expected in CI without NDK): ${e.message}")
            }
        }
    }

    fun isLoaded(): Boolean = loaded
}
