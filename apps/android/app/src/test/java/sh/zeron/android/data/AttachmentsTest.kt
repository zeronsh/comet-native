package sh.zeron.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AttachmentsTest {
    @Test
    fun withAttachmentsAppendsTrailer() {
        val text = withAttachments("Fix the diff", listOf("/home/u/a.png", "pending://u2/b.jpg"))
        assertTrue(text.startsWith("Fix the diff\n\nAttached images"))
        assertTrue(text.contains("- /home/u/a.png"))
        assertTrue(text.contains("- pending://u2/b.jpg"))
    }

    @Test
    fun emptyTextUsesAttachmentOnlyBody() {
        val text = withAttachments("", listOf("/x.png"))
        assertEquals(ATTACHMENT_ONLY_TEXT + "\n\nAttached images (local files — open them to view):\n- /x.png", text)
    }

    @Test
    fun parseRoundTripsOwnTrailer() {
        val original = "Look at this\n\nAttached images (local files — open them to view):\n- /home/u/photo.png\n- pending://abc/photo-1.jpg"
        val parsed = parseUserMessageImages(original)
        assertEquals("Look at this", parsed.text)
        assertEquals(2, parsed.attachments.size)
        assertEquals("photo.png", parsed.attachments[0].name)
        assertEquals("photo-1.jpg", parsed.attachments[1].name)
    }

    @Test
    fun parseIgnoresMarkerWithoutDashLines() {
        val noRefs = "Hello\n\nAttached images (local files — open them to view):\njust text"
        val parsed = parseUserMessageImages(noRefs)
        assertEquals(noRefs, parsed.text)
        assertTrue(parsed.attachments.isEmpty())
    }

    @Test
    fun attachmentOnlyBodyParsesToEmptyText() {
        val parsed = parseUserMessageImages(ATTACHMENT_ONLY_TEXT + "\n\nAttached images (local files — open them to view):\n- /a.png")
        assertEquals("", parsed.text)
        assertEquals(1, parsed.attachments.size)
    }

    @Test
    fun chunkSlicingUsesB64AlignedBoundary() {
        // 680000 b64 chars / 4 * 3 = 510000 binary bytes per chunk.
        assertEquals(680_000L / 4 * 3, 510_000L)
        val chunkBytes = UPLOAD_CHUNK_B64_CHARS / 4 * 3
        val size = 510_000 * 3 + 100
        val chunks = (size + chunkBytes - 1) / chunkBytes
        assertEquals(4, chunks)
        assertEquals(60, attachmentDeadlineSeconds(4))
        assertEquals(900L, attachmentDeadlineSeconds(100))
    }

    @Test
    fun stashRefsRoundTrip() {
        val ref = UploadStash.pendingRef("abc-123", "photo-x.jpg")
        assertEquals("pending://abc-123/photo-x.jpg", ref)
        val (id, name) = UploadStash.parseRef(ref)!!
        assertEquals("abc-123", id)
        assertEquals("photo-x.jpg", name)
        assertNull(UploadStash.parseRef("/not/pending/a.png"))
    }
}
