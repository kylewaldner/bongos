//! xref-history-list: the goto stack of the active pane (its backward and forward navigation
//! history) as a list in the minibuffer, newest place first, narrowed by what is typed. Enter
//! goes to the chosen place without changing either stack.

use bongos_minibuffer::{Candidate, Outcome, Session, Source, message, open};
use editor::Editor;
use gpui::{Action as _, App, Context, Entity, TaskExt as _, Window};
use multi_buffer::MultiBufferRow;
use text::Point;
use workspace::{HistoryRow, Pane, Workspace};

use crate::actions::XrefHistoryList;
use crate::last_command;

/// The name the prompt is routed under (`minibuffer.enabled_for`).
const MINIBUFFER_NAME: &str = "xref_history";
const STACK_IS_EMPTY: &str = "Goto stack is empty";

/// The trimmed text of the row of an open item, empty when the item is closed or has no such row.
fn line_text(row: &HistoryRow, cx: &App) -> String {
    let Some(row_ix) = row.row else {
        return String::new();
    };
    let Some(editor) = row
        .item
        .upgrade()
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return String::new();
    };
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    if row_ix > snapshot.max_point().row {
        return String::new();
    }
    let start = Point::new(row_ix, 0);
    let end = Point::new(row_ix, snapshot.line_len(MultiBufferRow(row_ix)));
    snapshot
        .text_for_range(start..end)
        .collect::<String>()
        .trim()
        .to_string()
}

fn candidate(row: &HistoryRow, cx: &App) -> Candidate {
    let path = row
        .project_path
        .as_ref()
        .map(|path| path.path.as_unix_str().to_string())
        .unwrap_or_else(|| "untitled".to_string());
    let primary = match row.row {
        Some(row_ix) => format!("{path}:{}", row_ix + 1),
        None => path,
    };
    Candidate::new(primary, row.timestamp).secondary(line_text(row, cx))
}

/// One candidate per place of `pane`'s goto stack, newest first.
pub(super) fn candidates(pane: &Entity<Pane>, cx: &App) -> Vec<Candidate> {
    pane.read(cx)
        .nav_history()
        .entries_newest_first(cx)
        .iter()
        .map(|row| candidate(row, cx))
        .collect()
}

fn xref_history_list(
    workspace: &mut Workspace,
    _: &XrefHistoryList,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    last_command::record(window, Some(XrefHistoryList::name_for_type()), cx);
    let pane = workspace.active_pane().clone();
    let candidates = candidates(&pane, cx);
    if candidates.is_empty() {
        message(window, cx, STACK_IS_EMPTY);
        return;
    }
    let weak_workspace = cx.entity().downgrade();
    let weak_pane = pane.downgrade();
    let session = Session::completing_read(MINIBUFFER_NAME, "Goto stack: ")
        .no_history()
        .source(Source::Fixed(candidates))
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(&timestamp) = value.downcast_ref::<usize>() else {
                return;
            };
            let Some(workspace) = weak_workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                workspace
                    .navigate_to_history_entry(weak_pane.clone(), timestamp, window, cx)
                    .detach_and_log_err(cx);
            });
        });
    open(workspace, session, window, cx);
}

/// Registers the `Workspace` handler on every workspace. The action is declared as ignoring the
/// prefix argument in `bongos_emacs::init`.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(xref_history_list);
    })
    .detach();
}
