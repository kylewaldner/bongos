package example;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class AppTest {
    @Test
    void adds() {
        assertEquals(3, App.add(1, 2));
    }
}
