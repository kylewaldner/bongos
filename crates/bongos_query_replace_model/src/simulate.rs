//! A whole session applied to a `String`: the oracle for the app's engine
//! tests and for the property tests.

use crate::session::{Effect, End, Found, Key, Notice, Phase, Resume, Session, continuation};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;

/// What a simulated session did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    pub text: String,
    pub end: Option<End>,
    /// Every match the session asked about, in the text as it was then.
    pub asked: Vec<Range<usize>>,
    /// Every match a `!` batch replaced, in the text as it was then.
    pub batch: Vec<Range<usize>>,
    /// Every range shown by `,` and `^`.
    pub shown: Vec<Range<usize>>,
    pub notices: Vec<Notice>,
}

/// Runs `session` over `text` from `start` and returns the final text.
/// `matcher(text, from)` returns the first match starting at or after `from`
/// (with `groups[0]` the matched text); the simulator fills in
/// `next_char_len` itself. `keys` are the decisions in order; when they run
/// out the session exits as with `q`. A `!` batch runs to its end.
pub fn simulate<M>(text: &str, start: usize, matcher: M, session: Session, keys: &[Key]) -> String
where
    M: FnMut(&str, usize) -> Option<Found>,
{
    trace(text, start, matcher, session, keys).text
}

/// [`simulate`], returning what happened on the way.
///
/// Panics when the matcher returns a match before `from`, past the end, or
/// splitting a character, or when an effect would split one: the simulator is
/// an oracle and must not paper over a broken matcher or session.
pub fn trace<M>(
    text: &str,
    start: usize,
    mut matcher: M,
    mut session: Session,
    keys: &[Key],
) -> Trace
where
    M: FnMut(&str, usize) -> Option<Found>,
{
    let mut trace = Trace {
        text: text.to_string(),
        ..Trace::default()
    };
    // id -> (start, inserted text, the text it replaced), for `u`.
    let mut edits: HashMap<u64, (usize, String, String)> = HashMap::new();
    let mut next_id = 1;
    let mut keys = keys.iter().copied();

    let first = find(&mut matcher, &trace.text, start);
    let mut effects: VecDeque<Effect> = session.on_match(first).into();
    note_asked(&session, &mut trace);
    loop {
        let Some(effect) = effects.pop_front() else {
            if let Phase::Done(end) = session.phase() {
                trace.end = Some(*end);
                break;
            }
            let (key, last) = match keys.next() {
                Some(key) => (key, false),
                None => (Key::Exit, true),
            };
            effects.extend(session.on_key(key));
            if last && effects.is_empty() {
                break;
            }
            continue;
        };
        match effect {
            Effect::Replace { at, with } => {
                let original = edit(&mut trace.text, at.clone(), &with);
                let inserted = at.start..at.start + with.len();
                edits.insert(next_id, (at.start, with, original));
                session.on_replaced(next_id, inserted);
                next_id += 1;
            }
            Effect::Find { from } => {
                let found = find(&mut matcher, &trace.text, from);
                effects.extend(session.on_match(found));
                note_asked(&session, &mut trace);
            }
            Effect::UndoLast { id } => {
                let (start, inserted, original) = edits
                    .remove(&id)
                    .unwrap_or_else(|| panic!("undo of unknown edit {id}"));
                let range = start..start + inserted.len();
                assert_eq!(
                    trace.text.get(range.clone()),
                    Some(inserted.as_str()),
                    "undo of edit {id} at {range:?} found other text"
                );
                edit(&mut trace.text, range, &original);
            }
            Effect::Show(range) => trace.shown.push(range),
            Effect::StartBatch { from } => {
                let mut at = Resume::start(from.start);
                let mut done = 0;
                while let Some(found) = find(&mut matcher, &trace.text, at.from) {
                    if at.passes_over(&found) {
                        match continuation(&found, 0) {
                            Some(next) => {
                                at = next;
                                continue;
                            }
                            None => break,
                        }
                    }
                    let with = session.replacement_for(&found);
                    edit(&mut trace.text, found.range.clone(), &with);
                    trace.batch.push(found.range.clone());
                    done += 1;
                    session.on_batch_progress(done);
                    match continuation(&found, with.len()) {
                        Some(next) => at = next,
                        None => break,
                    }
                }
                effects.extend(session.on_batch_end(false));
            }
            // A simulated batch runs to its end in one step, so there is
            // nothing to stop.
            Effect::StopBatch => {}
            Effect::Notice(notice) => trace.notices.push(notice),
            Effect::Finish(end) => trace.end = Some(end),
        }
    }
    trace
}

fn note_asked(session: &Session, trace: &mut Trace) {
    if *session.phase() == Phase::Asking
        && let Some(found) = session.current()
    {
        trace.asked.push(found.range.clone());
    }
}

fn find<M>(matcher: &mut M, text: &str, from: usize) -> Option<Found>
where
    M: FnMut(&str, usize) -> Option<Found>,
{
    if from > text.len() {
        return None;
    }
    assert!(
        text.is_char_boundary(from),
        "search from {from} splits a character"
    );
    let mut found = matcher(text, from)?;
    let range = found.range.clone();
    assert!(
        range.start >= from
            && range.start <= range.end
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end),
        "matcher returned {range:?} for a search from {from} in a text of {} bytes",
        text.len()
    );
    found.next_char_len = text
        .get(range.end..)
        .and_then(|rest| rest.chars().next())
        .map_or(0, char::len_utf8);
    Some(found)
}

// Replaces `range` with `with` and returns the text it replaced.
fn edit(text: &mut String, range: Range<usize>, with: &str) -> String {
    let original = text
        .get(range.clone())
        .unwrap_or_else(|| panic!("edit at {range:?} splits a character or passes the end"))
        .to_string();
    text.replace_range(range, with);
    original
}
