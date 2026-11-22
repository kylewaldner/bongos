use crate::labels::{Resolved, labels, resolve};

/// Which Avy command a session mimics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `avy-goto-char-timer`: read characters until Enter or the timer fires.
    CharTimer,
    /// `avy-goto-word-1`: the first character shows labels at once.
    Word,
    /// `avy-goto-line`: labels are shown as soon as the session starts.
    Line,
}

/// One key or event delivered to a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Text(char),
    Enter,
    Backspace,
    Cancel,
    /// Any key that is not text, Enter, Backspace or cancel.
    Other,
    /// The host's idle timer elapsed (only meaningful in `CharTimer` reading).
    TimerFired,
}

/// What the host must do after a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Restart the idle timer. The host re-echoes `char: <query>` after every
    /// handled key, derived from `Session::query`.
    RestartTimer,
    /// Draw the labels in `Session::labels`.
    ShowLabels,
    /// A label prefix (the string) was typed; redraw with only matching labels.
    Narrow(String),
    /// Jump to the candidate with this index.
    Jump(usize),
    /// End the session without jumping.
    Cancel,
    /// The query (the string) has no candidates; the session ended.
    NoMatches(String),
    /// Redraw the echo for the query (the string); the timer is not restarted.
    Echo(String),
    /// Nothing to do.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Reading the search query.
    Reading,
    /// Labels are shown and `prefix` of a label has been typed so far.
    Labelled { prefix: String },
}

/// Pure key state machine for the Avy-style jump commands.
///
/// After `Jump`, `Cancel` or `NoMatches` the session is finished and every
/// later step returns `Effect::None`.
#[derive(Clone, Debug)]
pub struct Session {
    mode: Mode,
    alphabet: Vec<char>,
    query: String,
    phase: Phase,
    labels: Vec<String>,
    finished: bool,
}

impl Session {
    /// Starts reading. For `Line` mode the host must call `start` next.
    pub fn new(mode: Mode, alphabet: Vec<char>) -> Self {
        Self {
            mode,
            alphabet,
            query: String::new(),
            phase: Phase::Reading,
            labels: Vec::new(),
            finished: false,
        }
    }

    /// Line mode only: shows labels for the (empty) query at once, returning
    /// `ShowLabels` or `NoMatches("")`. Other modes return `Effect::None`.
    pub fn start(&mut self, mut count: impl FnMut(&str) -> usize) -> Effect {
        if self.mode != Mode::Line || self.finished || !self.labels.is_empty() {
            return Effect::None;
        }
        self.show_labels(&mut count)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Labels for the candidates of the current query; empty while reading.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Feeds one input. `count(query)` returns the number of candidates for
    /// the query (always empty in `Line` mode).
    pub fn step(&mut self, input: Input, mut count: impl FnMut(&str) -> usize) -> Effect {
        if self.finished {
            return Effect::None;
        }
        match self.phase.clone() {
            Phase::Reading => self.step_reading(input, &mut count),
            Phase::Labelled { prefix } => self.step_labelled(input, prefix, &mut count),
        }
    }

    fn step_reading(&mut self, input: Input, count: &mut impl FnMut(&str) -> usize) -> Effect {
        match (self.mode, input) {
            (_, Input::Cancel) => self.finish(Effect::Cancel),
            // Line mode waits for `start`; nothing else is accepted before it.
            (Mode::Line, _) => Effect::None,
            (Mode::CharTimer, Input::Text(c)) => {
                self.query.push(c);
                Effect::RestartTimer
            }
            (Mode::CharTimer, Input::Enter | Input::TimerFired) => {
                if self.query.is_empty() {
                    Effect::None
                } else {
                    self.show_labels(count)
                }
            }
            (Mode::CharTimer, Input::Backspace) => {
                self.query.pop();
                Effect::Echo(self.query.clone())
            }
            (Mode::Word, Input::Text(c)) => {
                self.query.push(c);
                self.show_labels(count)
            }
            (Mode::Word, Input::TimerFired) => Effect::None,
            (_, Input::Enter | Input::Backspace | Input::Other) => self.finish(Effect::Cancel),
        }
    }

    fn step_labelled(
        &mut self,
        input: Input,
        prefix: String,
        count: &mut impl FnMut(&str) -> usize,
    ) -> Effect {
        match input {
            Input::TimerFired => Effect::None,
            Input::Text(c) => {
                let mut typed = prefix;
                typed.push(c);
                match resolve(&self.labels, &typed) {
                    Resolved::Chosen(index) => self.finish(Effect::Jump(index)),
                    Resolved::Prefix => {
                        self.phase = Phase::Labelled {
                            prefix: typed.clone(),
                        };
                        Effect::Narrow(typed)
                    }
                    Resolved::NoMatch => self.finish(Effect::Cancel),
                }
            }
            Input::Backspace if self.mode == Mode::CharTimer => {
                self.query.pop();
                if self.query.is_empty() {
                    self.labels.clear();
                    self.phase = Phase::Reading;
                    Effect::Echo(String::new())
                } else {
                    self.show_labels(count)
                }
            }
            Input::Backspace | Input::Enter | Input::Other | Input::Cancel => {
                self.finish(Effect::Cancel)
            }
        }
    }

    /// Computes labels for the current query and enters the labelled phase,
    /// or finishes with `NoMatches` when there are no candidates.
    fn show_labels(&mut self, count: &mut impl FnMut(&str) -> usize) -> Effect {
        let candidates = count(&self.query);
        self.labels = labels(candidates, &self.alphabet);
        self.phase = Phase::Labelled {
            prefix: String::new(),
        };
        if self.labels.is_empty() {
            let query = self.query.clone();
            self.finish(Effect::NoMatches(query))
        } else {
            Effect::ShowLabels
        }
    }

    fn finish(&mut self, effect: Effect) -> Effect {
        self.finished = true;
        effect
    }
}
