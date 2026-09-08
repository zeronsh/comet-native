package sh.zeron.android.core

object LoggingPolicy {
    fun safe(msg: String): String = msg
    fun redactedToken(): String = "***"
}
