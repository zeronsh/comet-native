package sh.zeron.android

import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import sh.zeron.android.loro.RealNativeLoroDoc

@RunWith(AndroidJUnit4::class)
class NativeSmokeTest {
    @Test fun nativeLoadDoesNotCrash() {
        NativeLoader.loadOnce()
        assertTrue(NativeLoader.isLoaded())
    }

    @Test fun nativeDocReadAppendRoundtrip() = runBlocking {
        val doc = RealNativeLoroDoc()
        val json0 = doc.getDeepValueJson()
        assertTrue(json0 == "{}" || json0 == "null")
        val id = doc.appendCommand("run", """{"text":"hi"}""", "android-instrumented").getValue("id") as String
        assertTrue(id.isNotBlank())
        val json1 = doc.getDeepValueJson()
        assertTrue("deep value should include command id '$id': $json1", json1.contains(id))
        doc.closeDoc()
    }
}