//! Replace mode of find in files. `M-%` marks the picker's prompt `[replace]`; RET on any row
//! then asks for the replacement, exports every match to a project search item and, once its
//! search has ended, starts a query replace over the whole results multibuffer, asking at each
//! match. `C-c C-r` asks for the replacement and then how many matches in how many files it is
//! about to replace, and replaces them all on `y`. The matches are found again on the results
//! as they are when the replace starts, so a file edited meanwhile is replaced as it is now. The
//! edits are the query replace's own: one undo step per buffer, buffers left modified and
//! unsaved. An export whose item is closed before its search ends starts no replace.

use bongos_isearch_model::CaseMode as SearchCase;
use bongos_minibuffer::{Outcome, Session};
use bongos_query_replace::prompts::to_history;
use bongos_query_replace::starters::{ScopedReplace, start_in_scope};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{App, Context, Entity, Focusable as _, Window};
use search::ProjectSearchView;
use search::project_search::ProjectSearch;
use workspace::Workspace;

use crate::find::engine::FindRequest;
use crate::find::export;
use crate::find::picker::{self, FindPicker};
use crate::registry;

/// The mark replace mode adds to the prompt, after the flags.
pub const PROMPT_MARK: &str = "[replace]";
/// The message when there is no match to replace.
pub const NOTHING_TO_REPLACE: &str = "Nothing to replace";

/// The prompt that asks for the replacement of `text`.
pub fn replacement_prompt(text: &str) -> String {
    format!("Replace {text} with: ")
}

/// The question `C-c C-r` asks before it replaces every match.
pub fn count_prompt(matches: usize, files: usize) -> String {
    let matches_word = if matches == 1 { "match" } else { "matches" };
    let files_word = if files == 1 { "file" } else { "files" };
    format!("Replace {matches} {matches_word} in {files} {files_word}? ")
}

/// Switches replace mode of the open find picker.
pub fn toggle(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    registry::in_session(window, cx, |session, window, cx| {
        if let Some(picker) = session.as_any().downcast_ref::<FindPicker>() {
            picker.set_replace_mode(!picker.replace_mode(), window, cx);
        }
    })
}

/// Asks for a replacement and how many matches it replaces, then replaces every match.
pub fn replace_all(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    registry::in_session(window, cx, |session, window, cx| {
        if let Some(picker) = session.as_any().downcast_ref::<FindPicker>() {
            ask(picker, true, workspace, window, cx);
        }
    })
}

/// Closes the picker and asks for the replacement of its query; `all` replaces every match after
/// a confirmation, else the decision view asks at each one. A picker whose search has ended
/// without a row replaces nothing and asks nothing; while its search runs, the replacement is
/// asked and the export's own search decides.
pub(crate) fn ask(
    picker: &FindPicker,
    all: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let request = picker.request();
    if let Some(handle) = picker.handle() {
        if all && picker.settled() && handle.selected(cx).is_none() {
            bongos_minibuffer::message(window, cx, NOTHING_TO_REPLACE);
            return;
        }
        handle.close(window, cx);
    }
    if request.text.is_empty() {
        bongos_minibuffer::message(window, cx, NOTHING_TO_REPLACE);
        return;
    }
    let weak = cx.entity().downgrade();
    let session = Session::string(picker::HOST, replacement_prompt(&request.text))
        .history(to_history(request.flags.regex))
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Input(to) = outcome else {
                return;
            };
            if let Some(workspace) = weak.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    export_and_replace(request, to, all, workspace, window, cx)
                });
            }
        });
    bongos_minibuffer::open(workspace, session, window, cx);
}

/// Exports `request` and, once its search has ended, replaces in its results.
fn export_and_replace(
    request: FindRequest,
    to: String,
    all: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(search) = export::search(&request, workspace, window, cx) else {
        bongos_minibuffer::message(window, cx, NOTHING_TO_REPLACE);
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        // `None`: the item was closed before its search ended, so nothing is replaced.
        let Some(search) = search.await else {
            return;
        };
        cx.update(|window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                ended(&workspace, &search, &request, to, all, window, cx);
            }
        })
        .ok();
    })
    .detach();
}

/// The export's search has ended: asks at each match, or asks once and replaces them all.
fn ended(
    workspace: &Entity<Workspace>,
    search: &Entity<ProjectSearch>,
    request: &FindRequest,
    to: String,
    all: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = results_editor(workspace, search, cx) else {
        return;
    };
    let (matches, files) = {
        let search = search.read(cx);
        (
            search.match_ranges.len(),
            search.excerpts.read(cx).all_buffers().len(),
        )
    };
    if matches == 0 {
        bongos_minibuffer::message(window, cx, NOTHING_TO_REPLACE);
        return;
    }
    if !all {
        start(&editor, request, to, true, window, cx);
        return;
    }
    let request = request.clone();
    let editor = editor.downgrade();
    workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::yes_or_no(
            workspace,
            picker::HOST,
            count_prompt(matches, files),
            false,
            move |yes, window, cx| {
                if yes && let Some(editor) = editor.upgrade() {
                    start(&editor, &request, to, false, window, cx);
                }
            },
            window,
            cx,
        )
    });
}

/// The results editor of the project search item that shows `search`.
fn results_editor(
    workspace: &Entity<Workspace>,
    search: &Entity<ProjectSearch>,
    cx: &App,
) -> Option<Entity<Editor>> {
    let excerpts = &search.read(cx).excerpts;
    workspace
        .read(cx)
        .items_of_type::<ProjectSearchView>(cx)
        .map(|view| view.read(cx).results_editor().clone())
        .find(|editor| editor.read(cx).buffer() == excerpts)
}

/// Selects the whole results multibuffer and starts the replace in it, on its text as it is now.
fn start(
    editor: &Entity<Editor>,
    request: &FindRequest,
    to: String,
    interactive: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let scope = {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        snapshot.anchor_before(MultiBufferOffset(0))..snapshot.anchor_after(snapshot.len())
    };
    window.focus(&editor.focus_handle(cx), cx);
    editor.update(cx, |editor, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_anchor_ranges([scope.clone()])
        })
    });
    // Smart case decided by the find rule, as the export's search decides it, so the replace
    // finds the matches the export listed.
    let case = if request.flags.case_sensitive(&request.text) {
        SearchCase::Sensitive
    } else {
        SearchCase::Insensitive
    };
    start_in_scope(
        editor,
        ScopedReplace {
            from: request.text.clone(),
            regexp: request.flags.regex,
            case,
            whole_word: request.flags.whole_word,
            scope,
            to,
            interactive,
        },
        window,
        cx,
    );
}

#[cfg(test)]
#[path = "replace_tests.rs"]
mod tests;
