package sh.zeron.android.data

import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import sh.zeron.android.loro.FakeLoroDoc

class SessionAdapterTest {
    private fun doc(json: String) = SessionAdapter(FakeLoroDoc(json))

    @Test fun emptyDocNoParts() = runTest {
        assertEquals(0, doc("{}").transcript().parts.size)
    }

    @Test fun parsesTextAndToolParts() = runTest {
        val json = """
            {"messages":[{"id":"m1","parts":[
              {"id":"p1","kind":"text","text":"hello"},
              {"id":"p2","kind":"tool","call":{"name":"bash"},"isError":false}
            ]}]}
        """.trimIndent()
        val ts = doc(json).transcript()
        assertEquals(2, ts.parts.size)
        assertTrue(ts.parts[0] is Part.Text)
        assertEquals("hello", (ts.parts[0] as Part.Text).text)
        assertTrue(ts.parts[1] is Part.Tool)
    }

    @Test fun errorPart() = runTest {
        val json = """{"messages":[{"id":"m1","parts":[{"id":"e1","kind":"error","message":"boom"}]}]}"""
        val ts = doc(json).transcript()
        assertEquals("boom", (ts.parts[0] as Part.Error).message)
    }

    @Test fun streamingEntryMarksTranscriptWorking() = runTest {
        val json = """
            {"messages":[
              {"id":"m1","role":"user","status":"complete","parts":[{"id":"p1","kind":"text","text":"hi"}]},
              {"id":"m2","role":"assistant","status":"streaming","parts":[{"id":"p2","kind":"text","text":"th"}]}
            ]}
        """.trimIndent()
        val ts = doc(json).transcript()
        assertTrue(ts.working)
        assertEquals(MessageStatus.Complete, ts.messages[0].status)
        assertEquals(MessageStatus.Streaming, ts.messages[1].status)
    }

    @Test fun finishedRunIsNotWorking() = runTest {
        val json = """
            {"messages":[{"id":"m1","status":"complete","parts":[{"id":"p1","kind":"text","text":"done"}]}]}
        """.trimIndent()
        assertFalse(doc(json).transcript().working)
    }

    /** A just-opened entry has no parts yet, so it never reaches `messages`. */
    @Test fun partlessStreamingEntryStillWorks() = runTest {
        val json = """
            {"messages":[
              {"id":"m1","status":"complete","parts":[{"id":"p1","kind":"text","text":"hi"}]},
              {"id":"m2","status":"streaming","parts":[]}
            ]}
        """.trimIndent()
        val ts = doc(json).transcript()
        assertEquals(1, ts.messages.size)
        assertTrue(ts.working)
    }

    /** An older stalled `streaming` entry must not pin the spinner on. */
    @Test fun onlyTheLastEntryDecidesWorking() = runTest {
        val json = """
            {"messages":[
              {"id":"m1","status":"streaming","parts":[{"id":"p1","kind":"text","text":"a"}]},
              {"id":"m2","status":"complete","parts":[{"id":"p2","kind":"text","text":"b"}]}
            ]}
        """.trimIndent()
        assertFalse(doc(json).transcript().working)
    }

    @Test fun missingStatusIsNotWorking() = runTest {
        val json = """{"messages":[{"id":"m1","parts":[{"id":"p1","kind":"text","text":"a"}]}]}"""
        val ts = doc(json).transcript()
        assertNull(ts.messages[0].status)
        assertFalse(ts.working)
    }

    @Test fun inputPartParsesQuestions() = runTest {
        val json = """
            {"messages":[{"id":"m1","parts":[{
              "id":"req-1","kind":"input","resolved":false,
              "questions":[
                {"id":"q1","header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false},
                {"id":"q2","header":"Tags","question":"Pick many","options":["X","Y"],"multiSelect":true}
              ]
            }]}]}
        """.trimIndent()
        val input = doc(json).transcript().messages[0].parts[0] as Part.Input
        assertEquals("req-1", input.id)
        assertFalse(input.resolved)
        assertEquals(2, input.questions.size)
        assertEquals("q1", input.questions[0].id)
        assertEquals("Pick one", input.questions[0].question)
        assertEquals(listOf("A", "B"), input.questions[0].options)
        assertFalse(input.questions[0].multiSelect)
        assertTrue(input.questions[1].multiSelect)
    }

    @Test fun unresolvedInputIsTheOpenRequest() = runTest {
        val json = """
            {"messages":[{"id":"m1","parts":[{
              "id":"req-1","kind":"input","resolved":false,
              "questions":[{"id":"q1","header":"H","question":"Pick one","options":["A"],"multiSelect":false}]
            }]}]}
        """.trimIndent()
        val request = doc(json).transcript().openInputRequest
        assertNotNull(request)
        assertEquals("req-1", request?.id)
    }

    @Test fun resolvedInputIsNotTheOpenRequest() = runTest {
        val json = """
            {"messages":[{"id":"m1","parts":[{
              "id":"req-1","kind":"input","resolved":true,
              "questions":[{"id":"q1","header":"H","question":"Pick one","options":["A"],"multiSelect":false}]
            }]}]}
        """.trimIndent()
        assertNull(doc(json).transcript().openInputRequest)
    }

    /** An empty question list can't be answered — it must not take the composer. */
    @Test fun questionlessInputIsNotTheOpenRequest() = runTest {
        val json = """{"messages":[{"id":"m1","parts":[{"id":"req-1","kind":"input","resolved":false,"questions":[]}]}]}"""
        assertNull(doc(json).transcript().openInputRequest)
    }

    /** The newest UNRESOLVED input wins: scanning reversed skips a resolved tail. */
    @Test fun newestUnresolvedInputWins() = runTest {
        val json = """
            {"messages":[
              {"id":"m1","parts":[{"id":"open","kind":"input","resolved":false,"questions":[{"id":"q","header":"H","question":"still open","options":[],"multiSelect":false}]}]},
              {"id":"m2","parts":[{"id":"done","kind":"input","resolved":true,"questions":[{"id":"q","header":"H","question":"answered","options":[],"multiSelect":false}]}]}
            ]}
        """.trimIndent()
        assertEquals("open", doc(json).transcript().openInputRequest?.id)
    }

    @Test fun malformedJsonYieldsEmpty() = runTest {
        // FakeLoroDoc returns raw json; bad JSON → empty, not crash
        val ts = SessionAdapter(FakeLoroDoc("{ not json }")).transcript()
        assertTrue(ts.parts.isEmpty())
    }
}