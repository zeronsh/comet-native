package sh.zeron.android.data

/**
 * Folder browsing for the add-space flow — the iOS NewSpaceSheet data
 * (FolderEntry/FolderListing, the engine's ListFolders reply).
 */
data class FolderEntry(
    val name: String,
    val isDir: Boolean,
    val isRepo: Boolean,
)

data class FolderListing(
    val path: String,
    val entries: List<FolderEntry>,
    val truncated: Boolean,
) {
    /** The parent path, computed client-side (iOS FolderListing.parent). */
    val parent: String?
        get() {
            if (!path.contains("/") || path == "/") return null
            val trimmed = path.substringBeforeLast("/")
            return if (trimmed.isEmpty()) "/" else trimmed
        }
}
