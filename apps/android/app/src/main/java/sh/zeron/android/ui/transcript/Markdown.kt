package sh.zeron.android.ui.transcript

import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import sh.zeron.android.ui.theme.ZeronColors

/**
 * The subset of Markdown an agent transcript actually emits. This replaces
 * `NoopMarkdownParser`, which returned the whole document as one paragraph and
 * so rendered `**bold**` and ``` fences as literal punctuation.
 *
 * Deliberately not a full CommonMark implementation: block quotes, tables,
 * reference links and nested lists fall through as plain paragraphs rather
 * than being rendered wrong.
 */
sealed interface MdBlock {
    data class Paragraph(val text: AnnotatedString) : MdBlock
    data class Heading(val level: Int, val text: AnnotatedString) : MdBlock
    data class Bullet(val items: List<AnnotatedString>) : MdBlock
    data class Code(val code: String, val lang: String?) : MdBlock
}

object Markdown {
    fun parse(source: String): List<MdBlock> {
        val blocks = mutableListOf<MdBlock>()
        val lines = source.replace("\r\n", "\n").split("\n")
        var i = 0
        val paragraph = StringBuilder()
        val bullets = mutableListOf<AnnotatedString>()

        fun flushParagraph() {
            if (paragraph.isNotEmpty()) {
                blocks += MdBlock.Paragraph(inline(paragraph.toString().trim()))
                paragraph.setLength(0)
            }
        }
        fun flushBullets() {
            if (bullets.isNotEmpty()) {
                blocks += MdBlock.Bullet(bullets.toList())
                bullets.clear()
            }
        }
        fun flushAll() { flushParagraph(); flushBullets() }

        while (i < lines.size) {
            val line = lines[i]
            val trimmed = line.trim()

            // Fenced code: everything up to the closing fence is verbatim.
            if (trimmed.startsWith("```")) {
                flushAll()
                val lang = trimmed.removePrefix("```").trim().ifEmpty { null }
                val body = StringBuilder()
                i++
                while (i < lines.size && !lines[i].trim().startsWith("```")) {
                    if (body.isNotEmpty()) body.append('\n')
                    body.append(lines[i])
                    i++
                }
                i++ // consume the closing fence (or run off the end, which is fine)
                blocks += MdBlock.Code(body.toString(), lang)
                continue
            }

            val heading = HEADING.matchEntire(trimmed)
            if (heading != null) {
                flushAll()
                blocks += MdBlock.Heading(
                    level = heading.groupValues[1].length.coerceAtMost(3),
                    text = inline(heading.groupValues[2]),
                )
                i++
                continue
            }

            val bullet = BULLET.matchEntire(trimmed)
            if (bullet != null) {
                flushParagraph()
                bullets += inline(bullet.groupValues[1])
                i++
                continue
            }

            if (trimmed.isEmpty()) {
                flushAll()
            } else {
                flushBullets()
                if (paragraph.isNotEmpty()) paragraph.append('\n')
                paragraph.append(trimmed)
            }
            i++
        }
        flushAll()
        return blocks
    }

    private val HEADING = Regex("^(#{1,6})\\s+(.*)$")
    private val BULLET = Regex("^[-*+]\\s+(.*)$")

    /**
     * Inline spans: `code`, **bold**, *italic*. An unclosed delimiter is left as
     * literal text rather than swallowing the rest of the paragraph.
     */
    fun inline(source: String): AnnotatedString = buildAnnotatedString {
        var i = 0
        while (i < source.length) {
            val rest = source.length - i
            when {
                source[i] == '`' -> {
                    val end = source.indexOf('`', i + 1)
                    if (end < 0) { append(source[i]); i++ } else {
                        withStyle(
                            SpanStyle(
                                fontFamily = FontFamily.Monospace,
                                color = ZeronColors.inlineCodeText,
                                background = ZeronColors.inlineCodeWash,
                            )
                        ) { append(source.substring(i + 1, end)) }
                        i = end + 1
                    }
                }
                rest >= 2 && source.startsWith("**", i) -> {
                    val end = source.indexOf("**", i + 2)
                    if (end < 0) { append(source[i]); i++ } else {
                        withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) {
                            append(source.substring(i + 2, end))
                        }
                        i = end + 2
                    }
                }
                source[i] == '*' || source[i] == '_' -> {
                    val delimiter = source[i]
                    val end = source.indexOf(delimiter, i + 1)
                    if (end < 0) { append(source[i]); i++ } else {
                        withStyle(SpanStyle(fontStyle = FontStyle.Italic)) {
                            append(source.substring(i + 1, end))
                        }
                        i = end + 1
                    }
                }
                else -> { append(source[i]); i++ }
            }
        }
    }
}
