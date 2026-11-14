//! The four commands that start a replace: `query-replace`, `query-replace-regexp`,
//! `replace-string` and `replace-regexp`. Each refuses a read-only buffer, keeps only the primary
//! cursor (and its region), asks for the strings, and searches from point or within the region.
//! With no match the echo area says `Nothing to replace`; with one the mark is pushed where the
//! command started, so popping the mark returns there. The interactive commands then open the
//! decision view on the first match; the other two replace every match.

use std::ops::Range;

use bongos_emacs::mark::push_mark;
use bongos_emacs::region;
use bongos_isearch::engine::{Cancel, build_query};
use bongos_isearch_model::{CaseMode, SearchSpec, compile};
use bongos_query_replace_model::case::preserves_case;
use bongos_query_replace_model::session::{Effect, Session};
use bongos_query_replace_model::template;
use bongos_query_replace_model::text::end_message;
use editor::{Editor, SelectionEffects};
use gpui::{Action, App, Entity, WeakEntity, Window};
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset as _};

use crate::actions::{QueryReplace, QueryReplaceRegexp, ReplaceRegexp, ReplaceString};
use crate::edits::READ_ONLY;
use crate::engine;
use crate::prompts::{self, Answer, Mode};
use crate::session::{BatchRequest, InteractiveRequest, batch_running, open, run_batch};

/// Registers the starters on every editor.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let weak = cx.entity().downgrade();
        register::<QueryReplace>(editor, &weak, false, true);
        register::<QueryReplaceRegexp>(editor, &weak, true, true);
        register::<ReplaceString>(editor, &weak, false, false);
        register::<ReplaceRegexp>(editor, &weak, true, false);
    })
    .detach();
}

fn register<A: Action>(
    editor: &mut Editor,
    weak: &WeakEntity<Editor>,
    regexp: bool,
    interactive: bool,
) {
    let weak = weak.clone();
    editor
        .register_action(move |_: &A, window, cx| {
            let Some(editor) = weak.upgrade() else {
                return;
            };
            start(
                &editor,
                Mode {
                    regexp,
                    interactive,
                },
                window,
                cx,
            );
        })
        .detach();
}

/// Where a replace starts, taken before the prompts open.
#[derive(Clone)]
struct Origin {
    /// Point when the command started; the mark is pushed here.
    point: Anchor,
    /// Where the search starts: point, or the start of the region.
    from: Anchor,
    /// The region spanning lines, which bounds the replace.
    scope: Option<Range<Anchor>>,
}

/// Starts a replace in `editor`: a full editor in a workspace; anywhere else the key goes on.
pub fn start(editor: &Entity<Editor>, mode: Mode, window: &mut Window, cx: &mut App) {
    let workspace = {
        let editor = editor.read(cx);
        if editor.mode().is_full() {
            editor.workspace()
        } else {
            None
        }
    };
    let Some(workspace) = workspace else {
        cx.propagate();
        return;
    };
    if editor.read(cx).read_only(cx) {
        bongos_minibuffer::message(window, cx, READ_ONLY);
        return;
    }
    // One replace at a time: a batch still running in this editor finishes first.
    if batch_running(editor.read(cx)) {
        return;
    }
    let origin = editor.update(cx, |editor, cx| collapse(editor, window, cx));
    let weak = editor.downgrade();
    workspace.update(cx, |workspace, cx| {
        prompts::prompt(
            workspace,
            editor,
            mode,
            CaseMode::Smart,
            move |answer, window, cx| {
                if let Some(editor) = weak.upgrade() {
                    answered(&editor, mode, origin, answer, window, cx);
                }
            },
            window,
            cx,
        )
    });
}

/// Keeps only the primary cursor, with its region, and turns mark mode off (the mark push needs
/// it off). A region within one line seeds the string and the search starts at its start, so the
/// selected occurrence is the first match; a region spanning lines bounds the replace.
fn collapse(editor: &mut Editor, window: &mut Window, cx: &mut gpui::Context<Editor>) -> Origin {
    let display = editor.display_snapshot(cx);
    let newest = editor.selections.newest::<MultiBufferOffset>(&display);
    let active = region::is_active(editor, &newest, cx) && newest.start != newest.end;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let spans_lines = active
        && snapshot
            .text_for_range(newest.start..newest.end)
            .any(|chunk| chunk.contains('\n'));
    let primary = editor.selections.newest_anchor().clone();
    editor.set_selection_mark_mode(false, cx);
    editor.change_selections(
        SelectionEffects::no_scroll().nav_history(false),
        window,
        cx,
        |s| s.select_anchors(vec![primary.clone()]),
    );
    Origin {
        point: primary.head(),
        from: if active {
            primary.start
        } else {
            primary.head()
        },
        scope: spans_lines.then(|| primary.start..primary.end),
    }
}

/// Both strings are known: finds the first match, then stops there (`query-replace`) or
/// replaces every match (`replace-string`).
fn answered(
    editor: &Entity<Editor>,
    mode: Mode,
    origin: Origin,
    answer: Answer,
    window: &mut Window,
    cx: &mut App,
) {
    let mut spec = SearchSpec::new(answer.regexp);
    spec.string = answer.from.clone();
    spec.lax = false;
    spec.case = answer.case;
    let query = match build_query(&compile(&spec)) {
        Ok(query) => query,
        Err(error) => {
            bongos_minibuffer::error(window, cx, error.0);
            return;
        }
    };
    let template = match template::parse(&answer.to, answer.regexp) {
        Ok(template) => template,
        Err(error) => {
            bongos_minibuffer::error(window, cx, error.to_string());
            return;
        }
    };
    let preserve_case = preserves_case(spec.case_sensitive(), &answer.from);

    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let from = origin.from.to_offset(&snapshot);
    let region = origin
        .scope
        .as_ref()
        .map(|scope| scope.start.to_offset(&snapshot)..scope.end.to_offset(&snapshot));
    let scope = engine::scope(&snapshot, from, region);
    let first = engine::next(&snapshot, &query, from, &scope, &Cancel::new());
    let Some(first) = first else {
        let mut model = Session::new(preserve_case, template);
        let message = model
            .on_match(None)
            .iter()
            .find_map(|effect| match effect {
                Effect::Finish(end) => Some(end_message(end)),
                _ => None,
            })
            .unwrap_or_default();
        bongos_minibuffer::message(window, cx, message);
        return;
    };

    editor.update(cx, |editor, cx| editor.set_selection_mark_mode(false, cx));
    push_mark(editor, Some(origin.point), false, window, cx);

    if mode.interactive {
        let workspace = editor.read(cx).workspace();
        if let Some(workspace) = workspace {
            open(
                editor,
                &workspace,
                InteractiveRequest {
                    query,
                    scope,
                    first,
                    preserve_case,
                    template,
                    from: answer.from,
                    regexp: answer.regexp,
                },
                window,
                cx,
            );
        }
        return;
    }
    run_batch(
        editor,
        BatchRequest {
            query,
            scope,
            first,
            preserve_case,
            template,
        },
        window,
        cx,
    );
}
