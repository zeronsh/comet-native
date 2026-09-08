package sh.zeron.android.core

sealed class AppError(message: String) : Exception(message) {
    class Auth(val code: Int, val body: String) : AppError("auth $code")
    class Transport(val code: Int?) : AppError("transport $code")
    class Protocol(val detail: String) : AppError("protocol: $detail")
    class Loro(val detail: String) : AppError("loro: $detail")
    class Authorization(val detail: String) : AppError("authz: $detail")
    class Cancelled : AppError("cancelled")
    object Retryable : AppError("retryable")
    object Permanent : AppError("permanent")

    val isRetryable: Boolean get() = this is Retryable || this is Transport
    val safeMessage: String get() = message ?: "unknown"
}
