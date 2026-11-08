//! `task: mode` (PRD, four modes): modes are things done to a task, not states of it. The picker
//! offers chat, run, manual and thinking on the focused task, or on a task picked by name when
//! none is focused.
//!
//! - Chat runs `chat: new` (feature 04) and run runs `workflow: run` (feature 20), both found by
//!   name, because this crate depends on neither. When the action is not handled yet, a toast
//!   says which feature provides it. They read the task from [`Focus`], so a task that is not
//!   focused is focused first.
//! - Run is refused on a group, and on a leaf that already has an active run (one run per leaf).
//! - Manual focuses the task's worktree or directory, like `task: focus`.
//! - Thinking is `task: think`: a new `untitled-<n>.md` in the task's directory and nothing else.

use bongos_work_model::derive::{self, Shape};
use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::task;
use crate::commands::{ready, show_toast, with_task};
use crate::focus::{self, AfterFocus};
use crate::registry::LiveModesRegistry;
use crate::{Focus, artifacts, modals};

/// The action of chat sessions, from feature 04.
pub(crate) const CHAT_ACTION: &str = "chat::New";
/// The toast shown while feature 04 is not there.
pub(crate) const CHAT_MISSING: &str = "Chat sessions arrive with feature 04";
/// The action of workflow runs, from feature 20.
pub(crate) const RUN_ACTION: &str = "workflow::Run";
/// The toast shown while feature 20 is not there.
pub(crate) const RUN_MISSING: &str = "Workflows arrive with feature 20";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Chat,
    Run,
    Manual,
    Thinking,
}

const MODES: [(Mode, &str); 4] = [
    (Mode::Chat, "Chat: an agent in a terminal"),
    (Mode::Run, "Run: a workflow with gates"),
    (Mode::Manual, "Manual: focus its worktree or directory"),
    (Mode::Thinking, "Thinking: a new document, no agent"),
];

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|w, _: &task::Mode, window, cx| mode(w, window, cx));
}

fn mode(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    if let Some(id) = artifacts::focused(&tree, cx) {
        pick_mode(id, workspace, window, cx);
        return;
    }
    with_task(
        workspace,
        window,
        cx,
        "Work which task?",
        "There is no task",
        |task, _| task.file.is_some(),
        Box::new(pick_mode),
    );
}

fn pick_mode(
    id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let labels = MODES.iter().map(|(_, label)| label.to_string()).collect();
    modals::pick(
        workspace,
        "Work the task in which mode?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            run_mode(MODES[index].0, &id, workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn run_mode(
    mode: Mode,
    id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    match mode {
        Mode::Thinking => artifacts::open_untitled(workspace, &tree, id, window, cx),
        Mode::Manual => focus::focus_task(id, workspace, window, cx),
        Mode::Chat => start_by_name(CHAT_ACTION, CHAT_MISSING, id, workspace, window, cx),
        Mode::Run => {
            let is_group = tree
                .task_by_id(id)
                .is_none_or(|task| derive::shape(task, &tree) == Shape::Group);
            if is_group {
                show_toast(
                    workspace,
                    "A run starts on a leaf; pick a leaf task".to_string(),
                    cx,
                );
                return;
            }
            let start = LiveModesRegistry::can_start_run(id, cx);
            if let Some(chip) = start.chip.filter(|_| !start.allowed) {
                show_toast(
                    workspace,
                    format!("{chip}; a leaf has one run at a time"),
                    cx,
                );
                return;
            }
            start_by_name(RUN_ACTION, RUN_MISSING, id, workspace, window, cx)
        }
    }
}

/// Dispatches the action `name` once the task `id` is focused, or shows `missing` when nothing
/// handles the action.
fn start_by_name(
    name: &str,
    missing: &str,
    id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let action = cx
        .build_action(name, None)
        .ok()
        .filter(|action| window.is_action_available(action.as_ref(), cx));
    let Some(action) = action else {
        show_toast(workspace, missing.to_string(), cx);
        return;
    };
    let dispatch: AfterFocus = Box::new(move |_, window, cx| window.dispatch_action(action, cx));
    let focused = Focus::global(cx).is_some_and(|focus| focus.read(cx).task_id() == Some(id));
    if focused {
        dispatch(workspace, window, cx);
    } else {
        focus::focus_task_then(id, Some(dispatch), workspace, window, cx);
    }
}
