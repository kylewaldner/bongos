/// One key press, without any UI types: the key name and the modifiers held. `key` is the
/// unshifted key as the platform names it: a single character (`x`, `4`, `-`, `%`), or a name such
/// as `enter`, `escape`, `space`, `tab`, `backspace`, `up`, `f1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keystroke {
    pub key: String,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// The super, command or windows key.
    pub platform: bool,
}

impl Keystroke {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..Self::default()
        }
    }

    pub fn control(mut self) -> Self {
        self.control = true;
        self
    }

    pub fn alt(mut self) -> Self {
        self.alt = true;
        self
    }

    pub fn shift(mut self) -> Self {
        self.shift = true;
        self
    }

    pub fn platform(mut self) -> Self {
        self.platform = true;
        self
    }

    fn notation(&self) -> String {
        let mut single_letter = self.key.chars();
        let single_letter = match (single_letter.next(), single_letter.next()) {
            (Some(c), None) if c.is_alphabetic() => Some(c),
            _ => None,
        };
        let has_other_modifier = self.control || self.alt || self.platform;
        // Shift on a bare letter is the capital letter, as Emacs writes it (`A`, not `S-a`).
        let capital = self.shift && !has_other_modifier && single_letter.is_some();
        let mut out = String::new();
        if self.control {
            out.push_str("C-");
        }
        if self.alt {
            out.push_str("M-");
        }
        if self.shift && !capital {
            out.push_str("S-");
        }
        if self.platform {
            out.push_str("s-");
        }
        match self.key.as_str() {
            "space" => out.push_str("SPC"),
            "enter" => out.push_str("RET"),
            "tab" => out.push_str("TAB"),
            "escape" => out.push_str("ESC"),
            "backspace" => out.push_str("DEL"),
            key if capital => out.extend(key.chars().flat_map(char::to_uppercase)),
            key if key.chars().count() == 1 => out.push_str(key),
            key => {
                out.push('<');
                out.push_str(key);
                out.push('>');
            }
        }
        out
    }
}

/// Writes a key sequence as Emacs does: `C-x 4 -`, `M-x`, `C-M-%`. Modifiers come in the order
/// `C-`, `M-`, `S-`, `s-`, keys are separated by a space, special keys read `RET`, `TAB`, `SPC`,
/// `ESC`, `DEL` or `<name>` (`<up>`, `<f1>`), and shift on a bare letter is the capital letter.
pub fn emacs_notation(keys: &[Keystroke]) -> String {
    keys.iter()
        .map(Keystroke::notation)
        .collect::<Vec<_>>()
        .join(" ")
}
