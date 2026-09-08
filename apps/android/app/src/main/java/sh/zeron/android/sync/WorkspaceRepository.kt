package sh.zeron.android.sync

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import sh.zeron.android.data.ChatRow
import sh.zeron.android.data.RegistryAdapter
import sh.zeron.android.loro.LoroDoc

class WorkspaceRepository(
    private val doc: LoroDoc,
    private val adapter: RegistryAdapter,
    private val sync: RegistrySync,
) {
    private val _chats = MutableStateFlow<List<ChatRow>>(emptyList())
    val chats: StateFlow<List<ChatRow>> = _chats

    suspend fun observe() { _chats.value = adapter.chats() }
    suspend fun archive(chatId: String, archived: Boolean) { /* write LWW archived field */ }
    suspend fun shutdown() { sync.stop(); doc.closeDoc() }
}
