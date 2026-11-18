//! The prefix argument as a pure state machine: C-u, M-digit and M-- presses
//! in, a value and an echo string out. No editor types.

/// How the argument was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// Started with C-u; `presses` counts the C-u presses (0 once a bare `-`
    /// started it is never reported: M-- followed by C-u becomes one press).
    Universal { presses: u32 },
    /// Started with M-digit.
    Numeric,
    /// Started with M--.
    Negative,
}

/// Emacs's raw prefix argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Raw {
    /// `(n)`: C-u pressed, n a power of four (negative after a minus).
    Multiple(i32),
    /// `-`: a bare minus.
    Minus,
    /// An integer typed with digits (or negated by M--).
    Number(i32),
    /// No argument: a minus pressed twice.
    Nil,
}

/// The argument as Emacs's `simple.el` keeps it (`universal-argument-more`, `negative-argument`
/// and `digit-argument`), so each key press follows Emacs. `open` says the digit and minus keys
/// are still taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixArg {
    raw: Raw,
    open: bool,
    kind: ArgKind,
}

const MAX: i32 = i32::MAX;

impl PrefixArg {
    /// C-u from nothing: 4.
    pub fn universal() -> Self {
        Self {
            raw: Raw::Multiple(4),
            open: true,
            kind: ArgKind::Universal { presses: 1 },
        }
    }

    /// M-digit from nothing.
    pub fn digit(d: u8) -> Self {
        Self {
            raw: Raw::Number(i32::from(d.min(9))),
            open: true,
            kind: ArgKind::Numeric,
        }
    }

    /// M-- from nothing: -1.
    pub fn negative() -> Self {
        Self {
            raw: Raw::Minus,
            open: true,
            kind: ArgKind::Negative,
        }
    }

    /// A further C-u. Multiplies a multiple of four by 4 (a bare minus becomes -4); on a number or
    /// no argument it ends the argument. Returns false (state unchanged) when it already ended.
    pub fn press_universal(&mut self) -> bool {
        if !self.open {
            return false;
        }
        match self.raw {
            Raw::Multiple(n) => {
                self.raw = Raw::Multiple(n.saturating_mul(4).clamp(-MAX, MAX));
                self.add_press();
            }
            Raw::Minus => {
                self.raw = Raw::Multiple(-4);
                self.kind = ArgKind::Universal { presses: 1 };
            }
            Raw::Number(_) | Raw::Nil => self.open = false,
        }
        true
    }

    fn add_press(&mut self) {
        self.kind = match self.kind {
            ArgKind::Universal { presses } => ArgKind::Universal {
                presses: presses.saturating_add(1),
            },
            _ => ArgKind::Universal { presses: 1 },
        };
    }

    /// A digit. Returns false (state unchanged) when digits are not accepted.
    pub fn press_digit(&mut self, d: u8) -> bool {
        if !self.accepts_digits() || d > 9 {
            return false;
        }
        let d = i32::from(d);
        self.raw = match self.raw {
            Raw::Number(n) => {
                let tens = n.saturating_mul(10);
                let next = if n < 0 {
                    tens.saturating_sub(d)
                } else {
                    tens.saturating_add(d)
                };
                Raw::Number(next.clamp(-MAX, MAX))
            }
            // A zero keeps the bare minus, so that -01 works as in Emacs.
            Raw::Minus if d == 0 => Raw::Minus,
            Raw::Minus => Raw::Number(-d),
            Raw::Multiple(_) | Raw::Nil => Raw::Number(d),
        };
        true
    }

    /// A bare `-` key. Returns false (state unchanged) when not accepted.
    pub fn press_minus(&mut self) -> bool {
        if !self.accepts_minus() {
            return false;
        }
        self.negative_argument();
        true
    }

    /// M--, never refused: negates a number, turns a bare minus into no argument, and makes
    /// anything else a bare minus. The digit and minus keys are taken again afterwards.
    pub fn negative_argument(&mut self) {
        self.raw = match self.raw {
            Raw::Number(n) => Raw::Number(-n),
            Raw::Minus => Raw::Nil,
            Raw::Multiple(_) | Raw::Nil => Raw::Minus,
        };
        self.open = true;
    }

    pub fn accepts_digits(&self) -> bool {
        self.open
    }

    /// The bare `-` key is accepted while the argument is open and not a typed number (after
    /// digits it is an ordinary character).
    pub fn accepts_minus(&self) -> bool {
        self.open && !matches!(self.raw, Raw::Number(_))
    }

    /// False only for no argument (a minus pressed twice).
    pub fn has_value(&self) -> bool {
        self.raw != Raw::Nil
    }

    /// Emacs's numeric value: a bare minus is -1, no argument is 1.
    pub fn value(&self) -> i32 {
        match self.raw {
            Raw::Multiple(n) | Raw::Number(n) => n,
            Raw::Minus => -1,
            Raw::Nil => 1,
        }
    }

    pub fn kind(&self) -> ArgKind {
        self.kind
    }

    pub fn echo(&self) -> String {
        match self.raw {
            Raw::Multiple(n) if n < 0 => format!("C-u ({n})"),
            Raw::Multiple(n) | Raw::Number(n) => format!("C-u {n}"),
            Raw::Minus => "C-u -".to_string(),
            Raw::Nil => String::new(),
        }
    }
}
