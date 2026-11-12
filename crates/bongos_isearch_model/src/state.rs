use crate::spec::{Direction, SearchSpec};
use std::ops::Range;

/// One resolved state of the search. While a step is `failing`, `found` keeps
/// the last match the view stays on (`None` when nothing matched yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub string: String,
    pub direction: Direction,
    pub found: Option<Range<usize>>,
    pub failing: bool,
    pub wrapped: bool,
    pub overwrapped: bool,
    pub regexp: bool,
}

/// Forward finds take the first match with `start >= from`; backward finds
/// take the last match with `end <= from`. A `from` past the end of the text
/// (`usize::MAX` for a wrapped backward search) means the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Find {
        generation: u64,
        spec: SearchSpec,
        from: usize,
        direction: Direction,
        wrap: bool,
    },
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AbortOutcome {
    /// The failing steps were popped; the string is the one to show again.
    TrimmedFailingSuffix(String),
    Quit,
}

struct Pending {
    string: String,
    direction: Direction,
    wrap: bool,
    wrapped: bool,
    overwrapped: bool,
}

pub struct Isearch {
    origin: usize,
    direction: Direction,
    spec: SearchSpec,
    steps: Vec<Step>,
    generation: u64,
    pending: Option<Pending>,
}

impl Isearch {
    pub fn new(origin: usize, direction: Direction, spec: SearchSpec) -> Self {
        let initial = Step {
            string: String::new(),
            direction,
            found: None,
            failing: false,
            wrapped: false,
            overwrapped: false,
            regexp: spec.regexp,
        };
        Self {
            origin,
            direction,
            spec,
            steps: vec![initial],
            generation: 0,
            pending: None,
        }
    }

    pub fn origin(&self) -> usize {
        self.origin
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn spec(&self) -> &SearchSpec {
        &self.spec
    }

    pub fn current(&self) -> &Step {
        self.steps.last().unwrap_or_else(|| unreachable!())
    }

    pub fn depth(&self) -> usize {
        self.steps.len()
    }

    /// The string was typed, yanked, backspaced or edited. Pops to the longest
    /// resolved step whose string is a prefix of it and searches on from
    /// there unless that step already has the string.
    pub fn set_string(&mut self, string: &str) -> Request {
        self.invalidate();
        let base = self
            .steps
            .iter()
            .rposition(|step| string.starts_with(&step.string))
            .unwrap_or(0);
        self.steps.truncate(base + 1);
        let step = &self.steps[base];
        self.spec.regexp = step.regexp;
        self.spec.string = string.to_string();
        if step.string == string {
            return Request::None;
        }
        let from = self.position(&self.steps[base], self.direction, Some(string.len()));
        let (wrapped, overwrapped) = (step.wrapped, step.overwrapped);
        self.request(
            string.to_string(),
            from,
            self.direction,
            false,
            wrapped,
            overwrapped,
        )
    }

    /// C-s or C-r. With an empty string it recalls `last`; otherwise it
    /// searches strictly past the current match, or wraps when the current
    /// step is failing in the same direction.
    pub fn repeat(&mut self, direction: Direction, last: Option<&str>) -> Request {
        self.invalidate();
        let top = self.current().clone();
        self.direction = direction;
        if top.string.is_empty() {
            let Some(last) = last.filter(|last| !last.is_empty()) else {
                return Request::None;
            };
            self.steps.truncate(1);
            self.spec.string = last.to_string();
            return self.request(
                last.to_string(),
                self.origin,
                direction,
                false,
                false,
                false,
            );
        }
        self.spec.string = top.string.clone();
        if top.failing && top.direction == direction {
            let from = match direction {
                Direction::Forward => 0,
                Direction::Backward => usize::MAX,
            };
            return self.request(
                top.string,
                from,
                direction,
                true,
                top.wrapped,
                top.overwrapped,
            );
        }
        let from = self.position(&top, direction, None);
        self.request(
            top.string,
            from,
            direction,
            false,
            top.wrapped,
            top.overwrapped,
        )
    }

    /// M-s M-<: the first match of the buffer.
    pub fn first(&mut self) -> Request {
        self.jump(Direction::Forward, 0)
    }

    /// M-s M->: the last match of the buffer.
    pub fn last(&mut self) -> Request {
        self.jump(Direction::Backward, usize::MAX)
    }

    /// Replaces the spec (a toggle) and searches its current string again from
    /// the current step's start position.
    pub fn set_spec(&mut self, mut spec: SearchSpec) -> Request {
        self.invalidate();
        let top = self.current().clone();
        spec.string = top.string.clone();
        self.spec = spec;
        if top.string.is_empty() {
            // Nothing is searched, so the first step takes the mode as well: it is what the
            // next string starts from.
            if let Some(first) = self.steps.last_mut() {
                first.regexp = self.spec.regexp;
            }
            return Request::None;
        }
        let from = self.position(&top, self.direction, Some(top.string.len()));
        self.request(
            top.string,
            from,
            self.direction,
            false,
            top.wrapped,
            top.overwrapped,
        )
    }

    /// Applies the result of the newest request. False means the result is
    /// stale and was ignored.
    pub fn apply(&mut self, generation: u64, found: Option<Range<usize>>) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let previous = self.current().found.clone();
        let step = match found {
            Some(range) => {
                let wrapped = pending.wrapped || pending.wrap;
                let passed_origin = match pending.direction {
                    Direction::Forward => range.start >= self.origin,
                    Direction::Backward => range.end <= self.origin,
                };
                Step {
                    string: pending.string,
                    direction: pending.direction,
                    found: Some(range),
                    failing: false,
                    wrapped,
                    overwrapped: pending.overwrapped || (wrapped && passed_origin),
                    regexp: self.spec.regexp,
                }
            }
            None => Step {
                string: pending.string,
                direction: pending.direction,
                found: previous,
                failing: true,
                wrapped: pending.wrapped,
                overwrapped: pending.overwrapped,
                regexp: self.spec.regexp,
            },
        };
        self.steps.push(step);
        true
    }

    /// C-g. In a failing state it pops every failing step; otherwise the
    /// session is to be quit.
    pub fn abort(&mut self) -> AbortOutcome {
        self.invalidate();
        if !self.current().failing {
            return AbortOutcome::Quit;
        }
        while self.steps.len() > 1 && self.current().failing {
            self.steps.pop();
        }
        let top = self.current().clone();
        self.spec.string = top.string.clone();
        self.spec.regexp = top.regexp;
        self.direction = top.direction;
        AbortOutcome::TrimmedFailingSuffix(top.string)
    }

    /// Moves every offset through `map` after a buffer change. A request in
    /// flight searched the old text, so its result is dropped.
    pub fn rebase(&mut self, map: impl Fn(usize) -> usize) {
        self.invalidate();
        self.origin = map(self.origin);
        for step in &mut self.steps {
            if let Some(range) = &mut step.found {
                *range = map(range.start)..map(range.end);
            }
        }
    }

    fn jump(&mut self, direction: Direction, from: usize) -> Request {
        self.invalidate();
        self.direction = direction;
        let string = self.current().string.clone();
        if string.is_empty() {
            return Request::None;
        }
        self.spec.string = string.clone();
        self.request(string, from, direction, false, false, false)
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.pending = None;
    }

    /// Where a search on from `step` starts. `new_length` is the length of
    /// the string for a new search of an extended or toggled string; a repeat
    /// passes `None` and starts past the match. A backward match is found by
    /// its start, so a new search starts at the start plus the new length, as
    /// Emacs does, to find the same match again when it still matches.
    fn position(&self, step: &Step, direction: Direction, new_length: Option<usize>) -> usize {
        let Some(range) = &step.found else {
            return self.origin;
        };
        match (direction, new_length) {
            (Direction::Forward, None) => range.end,
            (Direction::Forward, Some(_)) => range.start,
            (Direction::Backward, None) => range.start,
            (Direction::Backward, Some(length)) => range.end.max(range.start + length),
        }
    }

    fn request(
        &mut self,
        string: String,
        from: usize,
        direction: Direction,
        wrap: bool,
        wrapped: bool,
        overwrapped: bool,
    ) -> Request {
        self.generation += 1;
        let mut spec = self.spec.clone();
        spec.string = string.clone();
        self.pending = Some(Pending {
            string,
            direction,
            wrap,
            wrapped,
            overwrapped,
        });
        Request::Find {
            generation: self.generation,
            spec,
            from,
            direction,
            wrap,
        }
    }
}
