package sh.zeron.android.data

/**
 * A git ref on the host device (iOS Entities.RepoRef). `worktreePath` marks
 * refs already checked out in a worktree — reusing that checkout is cheaper
 * than creating another.
 */
data class RepoRef(
    val name: String,
    val current: Boolean = false,
    val worktreePath: String? = null,
)
