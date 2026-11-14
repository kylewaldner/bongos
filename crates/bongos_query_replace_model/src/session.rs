//! The query replace session: one decision per match, the stack of
//! replacements for `^` and `u`, the counts, and where the next search starts.
//!
//! The session knows nothing of buffers. Positions are opaque `usize` offsets
//! that the caller maps onto its text; the session only ever does arithmetic on
//! them that stays valid after its own edits (everything after a replaced
//! match shifts, everything before it stays).
//!
//! Protocol: the caller searches from the scope start and passes the result to
//! [`Session::on_match`]. From then on it applies the [`Effect`]s returned by
//! [`Session::on_match`], [`Session::on_key`] and the batch callbacks in
//! order: after applying a [`Effect::Replace`] it reports the edit with
//! [`Session::on_replaced`], and it answers every [`Effect::Find`] with
//! [`Session::on_match`] before sending the next key. Keys that arrive while a
//! find is pending are queued by the caller, not sent.

use crate::case;
use crate::template::Template;
use std::ops::Range;

/// A decision key, after the caller's keymap resolved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `y`, `SPC`: replace this match and go to the next.
    Yes,
    /// `n`, `DEL`: skip this match and go to the next.
    No,
    /// `!`: replace this and every remaining match without asking.
    All,
    /// `.`: replace this match and end the session.
    ThisAndExit,
    /// `,`: replace this match, show the result and wait.
    ThisAndWait,
    /// `^`: show the previous replacement, one older per press.
    Previous,
    /// `u`: undo the last replacement and ask about that match again.
    Undo,
    /// `q`, `RET`, `Escape`: end the session.
    Exit,
    /// `C-g`: end the session; replacements made stay. Interrupts a batch.
    Abort,
}

/// A match the caller found. `groups[0]` is the whole matched text and
/// `groups[i]` is capture group `i` (`None` when it did not participate); in
/// text mode `groups` is just the matched text.
///
/// `next_char_len` is the byte length of the character that starts at
/// `range.end`, 0 at the end of the text. The session is blind to the text, so
/// the caller supplies it: after an empty match the next search starts one
/// character later, and this keeps that step on a character boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub range: Range<usize>,
    pub groups: Vec<Option<String>>,
    pub next_char_len: usize,
}

impl Found {
    /// The whole matched text, `groups[0]`.
    pub fn matched(&self) -> &str {
        self.groups.first().and_then(|g| g.as_deref()).unwrap_or("")
    }
}

/// Where the session is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for [`Session::on_match`] (at the start and after every
    /// [`Effect::Find`]). Keys are ignored; the caller queues them.
    Finding,
    /// Asking about the current match.
    Asking,
    /// After `,`: the replacement is shown; the next key moves on.
    Shown,
    /// After `^`: showing the replacement `depth` entries down the stack
    /// (1 is the newest). Nothing is undone.
    Walking { depth: usize },
    /// After `!`: the caller replaces the rest; only `C-g` acts.
    Batch,
    /// The session is over.
    Done(End),
}

/// One interactive replacement, kept for `^` and `u`. `start..end` is the
/// inserted text; `id` is the caller's handle for the edit (its transaction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    pub id: u64,
    pub start: usize,
    pub end: usize,
    pub matched: String,
}

/// Something the caller shows in the status, see [`crate::text::notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// `^` with no older replacement.
    NoPrevious,
    /// `u` with no replacement left.
    NothingToUndo,
}

/// What the caller must do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Replace the text at `at` with `with`, then call [`Session::on_replaced`].
    Replace { at: Range<usize>, with: String },
    /// Search for the first match starting at or after `from` and pass it to
    /// [`Session::on_match`].
    Find { from: usize },
    /// Undo the edit with this id. Followed by a `Find` at the restored match.
    UndoLast { id: u64 },
    /// Show this range (a replacement, for `,` and `^`).
    Show(Range<usize>),
    /// Replace the match at `from` and every later one, reporting with
    /// [`Session::on_batch_progress`] and [`Session::on_batch_end`]. The scan
    /// starts at `Resume::start(from.start)`, passes over what
    /// [`Resume::passes_over`] names and goes on from [`continuation`].
    StartBatch { from: Range<usize> },
    /// Stop the running batch after the replacements already made, then call
    /// [`Session::on_batch_end`] with `interrupted` true.
    StopBatch,
    /// Show a short notice; the session stays where it is.
    Notice(Notice),
    /// The session is over.
    Finish(End),
}

/// Decisions made so far. A match whose replacement was undone with `u`
/// counts as neither until it is decided again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub replaced: usize,
    pub skipped: usize,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The first search found nothing.
    NoMatch,
    /// The search ran out of matches (no wrap).
    NoMore,
    /// `q`, `RET`, `Escape`.
    Exit,
    /// `.`
    ThisAndExit,
    /// `C-g` outside a batch.
    Aborted,
    /// `!` replaced everything.
    BatchDone,
    /// `C-g` stopped a batch.
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct End {
    pub reason: Reason,
    pub counts: Counts,
}

/// A search position between matches. `from` is where the next search
/// starts; `after_nonempty` says it is the end of a non-empty match, where an
/// empty match is passed over (Emacs `perform-replace`, and the `regex`
/// crate's iterators: no empty match right after the previous match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    pub from: usize,
    pub after_nonempty: bool,
}

impl Resume {
    /// The first search of a scan, from `from`.
    pub fn start(from: usize) -> Self {
        Self {
            from,
            after_nonempty: false,
        }
    }

    /// Whether `found`, the result of a search from here, must be passed
    /// over. The search then goes on from `continuation(found, 0)`.
    pub fn passes_over(&self, found: &Found) -> bool {
        self.after_nonempty && found.range.is_empty() && found.range.start == self.from
    }
}

/// Where the search goes on after `found`, given the length of what now
/// stands in its place (`found.range.len()` when it was skipped). Starting
/// after the inserted text means a replacement that contains the search
/// string is never searched again. After an empty match it is one character
/// later, so the same empty match is never found twice; `None` when that
/// character does not exist (the end of the text).
pub fn continuation(found: &Found, inserted_len: usize) -> Option<Resume> {
    let end = found.range.start + inserted_len;
    if found.range.is_empty() {
        (found.next_char_len > 0).then_some(Resume {
            from: end + found.next_char_len,
            after_nonempty: false,
        })
    } else {
        Some(Resume {
            from: end,
            after_nonempty: true,
        })
    }
}

struct Entry {
    replaced: Replaced,
    // Matches skipped before this replacement; `u` restores it because every
    // match after the undone one is decided again.
    skipped_before: usize,
}

pub struct Session {
    preserve_case: bool,
    template: Template,
    phase: Phase,
    // The match being asked about. Kept while walking with `^`, so a key
    // there acts on it.
    current: Option<Found>,
    // In `Shown`: where the search goes on (`None`: nothing is left).
    resume_from: Option<Resume>,
    // The search in flight.
    pending: Option<Resume>,
    // `!` pressed in `Shown`: the next match found starts the batch.
    batch_on_next: bool,
    batch_base: usize,
    stack: Vec<Entry>,
    counts: Counts,
    seen_match: bool,
}

impl Session {
    /// `preserve_case` is [`case::preserves_case`] for the session's search.
    pub fn new(preserve_case: bool, template: Template) -> Self {
        Self {
            preserve_case,
            template,
            phase: Phase::Finding,
            current: None,
            resume_from: None,
            pending: None,
            batch_on_next: false,
            batch_base: 0,
            stack: Vec::new(),
            counts: Counts::default(),
            seen_match: false,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The match being asked about (also while walking back with `^`).
    pub fn current(&self) -> Option<&Found> {
        self.current.as_ref()
    }

    /// The interactive replacements still undoable, oldest first.
    pub fn replaced(&self) -> impl DoubleEndedIterator<Item = &Replaced> {
        self.stack.iter().map(|entry| &entry.replaced)
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The text that replaces `found`: the template expanded with `\#` as the
    /// number of replacements made so far, then the match's case carried over.
    pub fn replacement_for(&self, found: &Found) -> String {
        self.replacement_numbered(found, self.counts.replaced)
    }

    /// [`Self::replacement_for`] with an explicit `\#`, for a batch computed
    /// ahead of [`Self::on_batch_progress`].
    pub fn replacement_numbered(&self, found: &Found, counter: usize) -> String {
        let groups: Vec<Option<&str>> = found.groups.iter().map(|g| g.as_deref()).collect();
        let expanded = self.template.expand(&groups, counter);
        case::transfer(self.preserve_case, found.matched(), &expanded)
    }

    /// The result of the last [`Effect::Find`] (or of the first search).
    /// Ignored unless a find is pending.
    pub fn on_match(&mut self, found: Option<Found>) -> Vec<Effect> {
        if self.phase != Phase::Finding {
            return Vec::new();
        }
        let Some(found) = found else {
            let reason = if self.seen_match {
                Reason::NoMore
            } else {
                Reason::NoMatch
            };
            return self.finish(reason);
        };
        if self
            .pending
            .take()
            .is_some_and(|pending| pending.passes_over(&found))
        {
            return self.search(continuation(&found, 0));
        }
        self.seen_match = true;
        if std::mem::take(&mut self.batch_on_next) {
            return self.start_batch(found.range);
        }
        self.phase = Phase::Asking;
        self.current = Some(found);
        Vec::new()
    }

    /// A decision key. Keys with no meaning in the current phase return no
    /// effects and change nothing.
    pub fn on_key(&mut self, key: Key) -> Vec<Effect> {
        match self.phase {
            Phase::Asking => self.key_asking(key),
            Phase::Shown => self.key_shown(key),
            Phase::Walking { depth } => self.key_walking(depth, key),
            Phase::Batch if key == Key::Abort => vec![Effect::StopBatch],
            Phase::Batch | Phase::Finding | Phase::Done(_) => Vec::new(),
        }
    }

    /// The caller applied the newest [`Effect::Replace`]: `id` is its handle
    /// for `u` and `new_range` the inserted text.
    pub fn on_replaced(&mut self, id: u64, new_range: Range<usize>) {
        if let Some(entry) = self.stack.last_mut() {
            entry.replaced.id = id;
            entry.replaced.start = new_range.start;
            entry.replaced.end = new_range.end;
        }
    }

    /// `done` replacements of the batch are made so far.
    pub fn on_batch_progress(&mut self, done: usize) {
        if self.phase == Phase::Batch {
            self.counts.replaced = self.batch_base + done;
        }
    }

    /// The batch ended, by itself or after [`Effect::StopBatch`].
    pub fn on_batch_end(&mut self, interrupted: bool) -> Vec<Effect> {
        if self.phase != Phase::Batch {
            return Vec::new();
        }
        self.finish(if interrupted {
            Reason::Interrupted
        } else {
            Reason::BatchDone
        })
    }

    fn key_asking(&mut self, key: Key) -> Vec<Effect> {
        let Some(found) = self.current.clone() else {
            return Vec::new();
        };
        match key {
            Key::Yes => {
                let (replace, inserted) = self.replace(&found);
                let mut effects = vec![replace];
                effects.extend(self.search(continuation(&found, inserted.len())));
                effects
            }
            Key::No => {
                self.counts.skipped += 1;
                self.search(continuation(&found, found.range.len()))
            }
            Key::All => self.start_batch(found.range),
            Key::ThisAndExit => {
                let (replace, _) = self.replace(&found);
                let mut effects = vec![replace];
                effects.extend(self.finish(Reason::ThisAndExit));
                effects
            }
            Key::ThisAndWait => {
                let (replace, inserted) = self.replace(&found);
                self.current = None;
                self.resume_from = continuation(&found, inserted.len());
                self.phase = Phase::Shown;
                vec![replace, Effect::Show(inserted)]
            }
            Key::Previous => self.walk(1),
            Key::Undo => self.undo(),
            Key::Exit => self.finish(Reason::Exit),
            Key::Abort => self.finish(Reason::Aborted),
        }
    }

    // After `,` the match is already replaced and counted, so `y` and `n`
    // both just move on, as in Emacs; `.` exits and `,` has nothing to do.
    fn key_shown(&mut self, key: Key) -> Vec<Effect> {
        match key {
            Key::Yes | Key::No => self.search(self.resume_from),
            Key::All => {
                self.batch_on_next = self.resume_from.is_some();
                self.search(self.resume_from)
            }
            Key::ThisAndExit | Key::Exit => self.finish(Reason::Exit),
            Key::ThisAndWait => Vec::new(),
            // The newest replacement is the one already shown.
            Key::Previous => self.walk(2),
            Key::Undo => self.undo(),
            Key::Abort => self.finish(Reason::Aborted),
        }
    }

    // Walking only shows; any decision key returns to the phase the walk
    // started from (the match asked about, or the `,` result) and acts there.
    fn key_walking(&mut self, depth: usize, key: Key) -> Vec<Effect> {
        match key {
            Key::Previous => self.walk(depth + 1),
            Key::Exit => self.finish(Reason::Exit),
            Key::Abort => self.finish(Reason::Aborted),
            _ => {
                self.phase = if self.current.is_some() {
                    Phase::Asking
                } else {
                    Phase::Shown
                };
                let effects = self.on_key(key);
                if effects.is_empty() {
                    self.phase = Phase::Walking { depth };
                }
                effects
            }
        }
    }

    fn replace(&mut self, found: &Found) -> (Effect, Range<usize>) {
        let with = self.replacement_for(found);
        let inserted = found.range.start..found.range.start + with.len();
        self.stack.push(Entry {
            replaced: Replaced {
                id: 0,
                start: inserted.start,
                end: inserted.end,
                matched: found.matched().to_string(),
            },
            skipped_before: self.counts.skipped,
        });
        self.counts.replaced += 1;
        self.current = None;
        let effect = Effect::Replace {
            at: found.range.clone(),
            with,
        };
        (effect, inserted)
    }

    fn search(&mut self, resume: Option<Resume>) -> Vec<Effect> {
        self.current = None;
        self.resume_from = None;
        match resume {
            Some(resume) => {
                self.phase = Phase::Finding;
                self.pending = Some(resume);
                vec![Effect::Find { from: resume.from }]
            }
            None => self.finish(Reason::NoMore),
        }
    }

    fn walk(&mut self, depth: usize) -> Vec<Effect> {
        let Some(entry) = self
            .stack
            .len()
            .checked_sub(depth)
            .and_then(|index| self.stack.get(index))
        else {
            return vec![Effect::Notice(Notice::NoPrevious)];
        };
        let range = entry.replaced.start..entry.replaced.end;
        self.phase = Phase::Walking { depth };
        vec![Effect::Show(range)]
    }

    fn undo(&mut self) -> Vec<Effect> {
        let Some(entry) = self.stack.pop() else {
            return vec![Effect::Notice(Notice::NothingToUndo)];
        };
        self.counts.replaced = self.counts.replaced.saturating_sub(1);
        self.counts.skipped = entry.skipped_before;
        self.current = None;
        self.resume_from = None;
        self.phase = Phase::Finding;
        self.pending = Some(Resume::start(entry.replaced.start));
        // The restored match is re-found from its start, so it is asked about
        // again with fresh groups, and every match after it is decided again.
        vec![
            Effect::UndoLast {
                id: entry.replaced.id,
            },
            Effect::Find {
                from: entry.replaced.start,
            },
        ]
    }

    fn start_batch(&mut self, from: Range<usize>) -> Vec<Effect> {
        self.current = None;
        self.batch_base = self.counts.replaced;
        self.phase = Phase::Batch;
        vec![Effect::StartBatch { from }]
    }

    fn finish(&mut self, reason: Reason) -> Vec<Effect> {
        self.current = None;
        self.resume_from = None;
        let end = End {
            reason,
            counts: self.counts,
        };
        self.phase = Phase::Done(end);
        vec![Effect::Finish(end)]
    }
}
