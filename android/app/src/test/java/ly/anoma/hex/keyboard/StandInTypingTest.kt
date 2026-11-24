package ly.anoma.hex.keyboard

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import ly.anoma.hex.typing.KeyGrid
import ly.anoma.hex.typing.KeyPoint
import ly.anoma.hex.typing.PersonalWords
import ly.anoma.hex.typing.SpatialDecoder
import ly.anoma.hex.typing.Suggestion
import ly.anoma.hex.typing.TypingModel
import ly.anoma.hex.typing.TypingWord

/** English's word list standing in for a dictation language with no model of its own (Português, Nederlands,
 * Polski), and literal Russian taps alongside the swipe model. */
class StandInTypingTest {
    private fun word(standIn: Boolean = false, vocabulary: List<String> = emptyList(), language: String = "en"): TypingWord {
        val model = TypingModel.file(File("src/main/assets/typing/$language.hextm"), language)
        return TypingWord(SpatialDecoder(model), spatial = true, vocabulary = vocabulary).also {
            it.decoder.personal = PersonalWords(model, listOf("Zorbex"), mapOf("myem" to "my@email.com"))
            it.standIn = standIn
        }
    }

    private val grid = KeyGrid(buildMap {
        for ((row, letters) in listOf("qwertyuiop", "asdfghjkl", "zxcvbnm").withIndex())
            for ((i, c) in letters.withIndex()) put(c, KeyPoint(201 + (i - (letters.length - 1) / 2.0) * 39.5, 66.5 + 56 * row))
    }, 39.5, 56.0)

    private fun TypingWord.type(text: String) {
        for (c in text) {
            val p = grid.centres.getValue(c)
            letter(c.toString(), p.x, p.y, grid)
        }
    }

    @Test fun standInOffersYourWordsAndShortcutsButNoEnglishCompletionsOrNextWords() {
        val english = word()
        english.explicit("hel")
        assertTrue("English completes a word being typed", english.strip().any { it.kind == Suggestion.Kind.Word })
        assertTrue("English predicts the next word", english.predictions("the ").isNotEmpty())

        val standIn = word(standIn = true)
        standIn.explicit("hel")
        assertEquals(listOf(Suggestion("hel", Suggestion.Kind.Typed)), standIn.strip())
        assertTrue(standIn.predictions("the ").isEmpty())
        // Your own word still completes, keeping the spelling you saved.
        standIn.reset(); standIn.explicit("zor")
        assertEquals(listOf(Suggestion("zor", Suggestion.Kind.Typed), Suggestion("Zorbex", Suggestion.Kind.Word)), standIn.strip())
        // Your shortcut still shows its phrase and still goes in on space.
        standIn.reset(); standIn.explicit("myem")
        assertEquals(listOf("myem", "my@email.com"), standIn.strip().map { it.text })
        assertEquals("my@email.com", standIn.textReplacement("myem")?.replacement)
    }

    @Test fun standInTurnsOffAutocorrectButKeepsYourTextReplacements() {
        val english = word()
        english.type("dont")
        assertEquals("don't", english.replacement())

        val standIn = word(standIn = true)
        standIn.type("dont")
        assertNull(standIn.replacement())
        assertNull(standIn.finish(' '))
        // Nor does a mark, from the letters or the ?123 page (Русский, Polski, Nederlands, Português: #2603).
        for (mark in ",.!?;:)\"") { standIn.reset(); standIn.type("teh"); assertNull("$mark", standIn.finish(mark)) }
        // Nothing changes what you typed on space or a mark, your own spellings included.
        standIn.reset(); standIn.type("zorbex")
        assertNull(standIn.replacement())
        // Your text replacement still goes in, whatever the language dictates.
        assertEquals("my@email.com", standIn.textReplacement("myem")?.replacement)
    }

    @Test fun cyrillicKeysKeepTheTypedWordWithoutEnglishAssistance() {
        val cyrillicGrid = KeyGrid(KeyboardLetters.rows("ru").flatMapIndexed { row, letters ->
            letters.mapIndexed { index, letter -> letter to KeyPoint(index * 34.0, row * 56.0) }
        }.toMap(), 34.0, 56.0)
        val standIn = word(language = "ru")
        for (letter in "Подъём") {
            if (letter == 'ё') standIn.explicit(letter.toString())
            else {
                val point = cyrillicGrid.centres.getValue(letter.lowercaseChar())
                standIn.letter(letter.toString(), point.x, point.y, cyrillicGrid)
            }
        }
        assertEquals(listOf(Suggestion("Подъём", Suggestion.Kind.Typed)), standIn.strip())
        assertNull(standIn.finish(' '))
        assertNull(standIn.finish('.'))
        assertTrue(standIn.predictions("Подъём ").isEmpty())
    }

    @Test fun russianPrivateVocabularyCompletesWithoutFixingThePrefix() {
        val standIn = word(language = "ru", vocabulary = listOf("ЗорбоЛист", "TypoBloom"))
        standIn.explicit("зо")
        assertEquals(listOf(Suggestion("зо", Suggestion.Kind.Typed), Suggestion("ЗорбоЛист", Suggestion.Kind.Word)),
            standIn.strip())
        assertNull(standIn.finish(' '))
        assertTrue(standIn.predictions("зо ").isEmpty())
    }

    @Test fun englishPrivateVocabularyKeepsProductCapitalization() {
        val english = word(vocabulary = listOf("iTovex Lab")).also { it.addsCapitals = false }
        english.explicit("It")
        assertTrue(english.strip().contains(Suggestion("iTovex Lab", Suggestion.Kind.Word)))
        english.restored("itovex", "")
        assertTrue(english.strip().contains(Suggestion("iTovex Lab", Suggestion.Kind.Word)))
        val product = word(vocabulary = listOf("TypoBloom"))
        product.explicit("typobloom")
        assertEquals(Suggestion("TypoBloom", Suggestion.Kind.Word), product.strip()[1])
        assertNull(product.finish(' '))
    }

    @Test fun ordinaryRussianCompletionsKeepTheCaseThatWasTyped() {
        val word = word(language = "ru", vocabulary = listOf("берёза"))
        for ((prefix, expected) in listOf("бе" to "берёза", "Бе" to "Берёза", "БЕ" to "БЕРЁЗА")) {
            word.restored(prefix, "")
            assertTrue(word.strip().contains(Suggestion(expected, Suggestion.Kind.Word)))
        }
        word.restored("Берёза", "")
        assertEquals(listOf(Suggestion("Берёза", Suggestion.Kind.Typed)), word.strip())
    }
}
