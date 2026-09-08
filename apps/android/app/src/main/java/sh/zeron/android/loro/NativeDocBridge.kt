package sh.zeron.android.loro

import sh.zeron.android.NativeLoader

/**
 * JNI bridge to the zeron-loro-android C library. The Rust side exports
 * `Java_sh_zeron_android_loro_NativeDocBridge_*` (JNI name mangling of this
 * package+class). `System.loadLibrary("zeron_loro_android")` must have been
 * called first (NativeLoader.loadOnce). Handles cross as jlong.
 */
object NativeDocBridge {
    init {
        if (!NativeLoader.isLoaded()) NativeLoader.loadOnce()
    }

    @JvmStatic external fun createDoc(): Long
    @JvmStatic external fun readJson(handle: Long): String
    @JvmStatic external fun import(handle: Long, hexBytes: String): Int
    @JvmStatic external fun exportHex(handle: Long): String
    @JvmStatic external fun appendCommand(handle: Long, commandJson: String): Int
    @JvmStatic external fun free(handle: Long)
}