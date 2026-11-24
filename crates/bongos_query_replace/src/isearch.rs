//! The hand-off from an incremental search: `isearch::QueryReplace` and
//! `isearch::QueryReplaceRegexp` end the search at the start of the match and call
//! `start_from_isearch`, which keeps the search's string and settings, asks only for the
//! replacement, and replaces from that match to the end of the buffer (or of the excerpt). The
//! search already pushed its origin onto the mark ring, so no mark is pushed here.

use std::rc::Rc;

use bongos_isearch::seams::{QueryReplaceSeed, set_query_replace_starter};
use bongos_isearch_model::{CaseMode, SearchSpec, to_input};
use bongos_minibuffer::HistoryStore;
use gpui::{App, Window};

use crate::prompts::{self, Mode, from_history};
use crate::starters::{Origin, begin, can_start};

/// Registers the hand-off with isearch.
pub(crate) fn init(cx: &mut App) {
    set_query_replace_starter(Rc::new(start_from_isearch), cx);
}

/// Starts an interactive replace from a search that ended: the search string is the FROM and goes
/// onto its history ring, and the replacement prompt opens at once.
pub fn start_from_isearch(seed: QueryReplaceSeed, window: &mut Window, cx: &mut App) {
    let Some(editor) = seed.editor.upgrade() else {
        return;
    };
    let workspace = {
        let editor = editor.read(cx);
        if editor.mode().is_full() {
            editor.workspace()
        } else {
            None
        }
    };
    let Some(workspace) = workspace else {
        return;
    };
    if seed.from.is_empty() || !can_start(&editor, window, cx) {
        return;
    }
    let mode = Mode {
        regexp: seed.regexp,
        interactive: true,
    };
    let case = match seed.case_sensitive {
        None => CaseMode::Smart,
        Some(true) => CaseMode::Sensitive,
        Some(false) => CaseMode::Insensitive,
    };
    let mut spec = SearchSpec::new(seed.regexp);
    spec.string = seed.from.clone();
    spec.symbol = seed.symbol;
    spec.case = case;
    spec.lax = false;
    HistoryStore::global(cx).push(from_history(mode.regexp), &to_input(&seed.from), cx);
    let origin = Origin {
        point: None,
        from: seed.start,
        scope: None,
    };
    let weak = editor.downgrade();
    workspace.update(cx, |workspace, cx| {
        prompts::prompt_replacement(
            workspace,
            mode,
            case,
            &seed.from,
            move |answer, window, cx| {
                if let Some(editor) = weak.upgrade() {
                    let mut spec = spec;
                    spec.case = answer.case;
                    begin(&editor, mode, origin, spec, false, answer.to, window, cx);
                }
            },
            window,
            cx,
        )
    });
}
