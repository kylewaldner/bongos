package example

import kotlin.test.Test
import kotlin.test.assertEquals

class MainTest {
    @Test
    fun shouts() = assertEquals("HELLO", shout())
}
