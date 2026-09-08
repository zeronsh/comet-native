package sh.zeron.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class HarnessCatalogTest {
    @Test
    fun defaultReasoningIsXhighWhenLadderHasIt() {
        val fable = HarnessCatalog.models("claude-code").first { it.id == "claude-fable-5" }
        assertEquals("xhigh", HarnessCatalog.defaultReasoning(fable))
        val haiku = HarnessCatalog.models("claude-code").first { it.id == "claude-haiku-4-5" }
        assertNull(HarnessCatalog.defaultReasoning(haiku)) // no ladder — no effort
    }

    @Test
    fun codexModelsCarryLadderLevels() {
        val spark = HarnessCatalog.models("codex").first { it.id == "gpt-5.3-codex-spark" }
        assertEquals(listOf("low", "medium", "high", "xhigh"), spark.reasoningLevels)
        val sol = HarnessCatalog.models("codex").first { it.id == "gpt-5.6-sol" }
        assertTrue("ultra" in sol.reasoningLevels)
    }

    @Test
    fun reasoningLabelsMatchiOS() {
        assertEquals("X-High", HarnessCatalog.reasoningLabel("xhigh"))
        assertEquals("Ultrathink", HarnessCatalog.reasoningLabel("ultrathink"))
        assertEquals("Minimal", HarnessCatalog.reasoningLabel("minimal"))
    }

    @Test
    fun defaultSelectionCarriesReasoning() {
        val sel = HarnessCatalog.defaultSelection()
        assertEquals("claude-code", sel.harness)
        assertEquals("xhigh", sel.reasoning)
    }
}
