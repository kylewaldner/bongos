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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixArg {
    kind: ArgKind,
    negative: bool,
    digits: Option<u32>,
    open: bool,
}

const MAX: u32 = i32::MAX as u32;

impl PrefixArg {
    /// C-u from nothing: 4.
    pub fn universal() -> Self {
        Self {
            kind: ArgKind::Universal { presses: 1 },
            negative: false,
            digits: None,
            open: true,
        }
    }

    /// M-digit from nothing.
    pub fn digit(d: u8) -> Self {
        Self {
            kind: ArgKind::Numeric,
            negative: false,
            digits: Some(u32::from(d.min(9))),
            open: true,
        }
    }

    /// M-- from nothing: -1.
    pub fn negative() -> Self {
        Self {
            kind: ArgKind::Negative,
            negative: true,
            digits: None,
            open: true,
        }
    }

    /// A further C-u. Multiplies by 4 while no digit was typed, otherwise ends
    /// the number. Returns false (state unchanged) when the number already ended.
    pub fn press_universal(&mut self) -> bool {
        if !self.open {
            return false;
        }
        if self.digits.is_some() {
            self.open = false;
            return true;
        }
        self.kind = match self.kind {
            ArgKind::Universal { presses } => ArgKind::Universal {
                presses: presses.saturating_add(1),
            },
            _ => ArgKind::Universal { presses: 1 },
        };
        true
    }

    /// A digit. Returns false (state unchanged) when digits are not accepted.
    pub fn press_digit(&mut self, d: u8) -> bool {
        if !self.accepts_digits() || d > 9 {
            return false;
        }
        let n = self.digits.unwrap_or(0);
        self.digits = Some(n.saturating_mul(10).saturating_add(u32::from(d)).min(MAX));
        true
    }

    /// A `-`. Returns false (state unchanged) when not accepted.
    pub fn press_minus(&mut self) -> bool {
        if !self.accepts_minus() {
            return false;
        }
        self.negative = true;
        true
    }

    pub fn accepts_digits(&self) -> bool {
        self.open
    }

    /// `-` is accepted only before any digit and while not already negative.
    pub fn accepts_minus(&self) -> bool {
        self.open && self.digits.is_none() && !self.negative
    }

    pub fn value(&self) -> i32 {
        let magnitude = self.magnitude() as i32;
        if self.negative { -magnitude } else { magnitude }
    }

    pub fn kind(&self) -> ArgKind {
        self.kind
    }

    pub fn echo(&self) -> String {
        if self.digits.is_none() && self.negative && self.magnitude() <= 4 {
            return "C-u -".to_string();
        }
        format!("C-u {}", self.value())
    }

    fn magnitude(&self) -> u32 {
        if let Some(digits) = self.digits {
            return digits;
        }
        let presses = match self.kind {
            ArgKind::Universal { presses } => presses,
            ArgKind::Numeric | ArgKind::Negative => 0,
        };
        let mut magnitude: u32 = 1;
        for _ in 0..presses {
            magnitude = magnitude.saturating_mul(4).min(MAX);
        }
        magnitude
    }
}
