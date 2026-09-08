package sh.zeron.android.config

import org.junit.Assert.*
import org.junit.Test

class AppConfigTest {
    @Test fun devAllowsHttp() {
        val c = AppConfig(edgeBaseUrl = "http://localhost:8787", authMode = AuthMode.Dev, deviceId = "d1")
        assertTrue(c.isDev)
    }
    @Test(expected = IllegalArgumentException::class)
    fun prodRejectsHttp() {
        AppConfig(edgeBaseUrl = "http://edge.zeron.sh", authMode = AuthMode.WorkOS, deviceId = "d1")
    }
    @Test fun validHttps() {
        val c = AppConfig(edgeBaseUrl = "https://edge.zeron.sh", authMode = AuthMode.WorkOS, deviceId = "d1")
        assertEquals("https://edge.zeron.sh", c.edgeBaseUrl)
    }
}
