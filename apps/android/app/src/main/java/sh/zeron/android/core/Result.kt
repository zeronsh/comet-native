package sh.zeron.android.core

sealed class AppResult<out T> {
    data class Ok<T>(val value: T) : AppResult<T>()
    data class Err(val error: AppError) : AppResult<Nothing>()
}
