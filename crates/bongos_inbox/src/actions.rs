//! The actions an item names and what `a` and `o` run. An item lists action names in its header;
//! a name is a key of the [`InboxActions`] registry, never a command: bongos runs nothing a file
//! names, so a name nobody registered is not shown and `a` does not run it.
//!
//! The built-in names are `open` (the item's file in a read-only tab), `dismiss` and `accept` and
//! `done` (archive the item), `overturn` (the item's target in a normal editor at its line, and
//! the item stays, read, until dismissed), `open-task` (focuses the task of a `task:<id>`
//! target) and `open-run` (shows the run of a `run:<id>` target). A handler says what becomes of
//! the item with an [`ActionOutcome`]. `inbox: open target` (`o`) opens the target of the item
//! whatever its form, see [`crate::targets`].

use crate::Inbox;
use crate::targets::{Located, TargetState};
use editor::Editor;
use gpui::{App, Global, TaskExt as _, WeakEntity, Window};
use language::{Buffer, Point};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use util::ResultExt as _;
use workspace::Workspace;

pub const OPEN: &str = "open";
pub const DISMISS: &str = "dismiss";
pub const ACCEPT: &str = "accept";
pub const DONE: &str = "done";
pub const OVERTURN: &str = "overturn";
pub const OPEN_TASK: &str = "open-task";
pub const OPEN_RUN: &str = "open-run";

/// What `a` says on an item that has no registered action.
pub const NO_PRIMARY_ACTION: &str = "no primary action";
/// What `a` and `o` say on a group row.
pub const EXPAND_FIRST: &str = "expand the group first";

/// The item an action runs on.
#[derive(Clone, Debug)]
pub struct ItemRef {
    pub unique: String,
    pub inode: u64,
    /// The item's file in the inbox.
    pub path: PathBuf,
    pub target: Option<String>,
    /// What the last scan found the target to be.
    pub target_state: Option<TargetState>,
    /// The names the item lists, registered or not.
    pub actions: Vec<String>,
    /// The workspace of the view the action ran in.
    pub workspace: WeakEntity<Workspace>,
}

/// What becomes of the item once its action has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The item stays as it is.
    Keep,
    /// The item is archived.
    Archive,
    /// The item is marked read.
    MarkRead,
}

pub type ActionHandler = Rc<dyn Fn(&ItemRef, &mut Window, &mut App) -> ActionOutcome>;
type FocusTask = Rc<dyn Fn(&str, &WeakEntity<Workspace>, &mut Window, &mut App)>;

/// The registry of action names and their handlers, a global.
pub struct InboxActions {
    handlers: HashMap<String, ActionHandler>,
    focus_task: Option<FocusTask>,
}

impl Global for InboxActions {}

impl Default for InboxActions {
    fn default() -> Self {
        let mut actions = Self {
            handlers: HashMap::new(),
            focus_task: None,
        };
        actions.insert(OPEN, open);
        actions.insert(DISMISS, |_, _, _| ActionOutcome::Archive);
        actions.insert(ACCEPT, |_, _, _| ActionOutcome::Archive);
        actions.insert(DONE, |_, _, _| ActionOutcome::Archive);
        actions.insert(OVERTURN, overturn);
        actions.insert(OPEN_TASK, open_task);
        actions.insert(OPEN_RUN, open_run);
        actions
    }
}

impl InboxActions {
    fn insert(
        &mut self,
        name: &str,
        handler: impl Fn(&ItemRef, &mut Window, &mut App) -> ActionOutcome + 'static,
    ) {
        self.handlers.insert(name.to_string(), Rc::new(handler));
    }

    /// Registers `handler` under `name`, replacing any earlier one, built-ins included.
    pub fn register(
        name: &str,
        handler: impl Fn(&ItemRef, &mut Window, &mut App) -> ActionOutcome + 'static,
        cx: &mut App,
    ) {
        cx.default_global::<Self>().insert(name, handler);
    }

    /// Tells `open-task` how to focus a task (the app has the work crate do it).
    pub fn set_task_focus(
        focus: impl Fn(&str, &WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
        cx: &mut App,
    ) {
        cx.default_global::<Self>().focus_task = Some(Rc::new(focus));
    }

    /// Whether `name` has a handler.
    pub fn is_registered(name: &str, cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|actions| actions.handlers.contains_key(name))
    }

    pub(crate) fn handler(name: &str, cx: &App) -> Option<ActionHandler> {
        cx.try_global::<Self>()
            .and_then(|actions| actions.handlers.get(name).cloned())
    }
}

/// `open`: the item's file in a read-only tab.
fn open(item: &ItemRef, window: &mut Window, cx: &mut App) -> ActionOutcome {
    open_in_editor(&item.workspace, item.path.clone(), None, true, window, cx);
    ActionOutcome::Keep
}

/// `overturn`: the target in a normal, writable editor at its line. The item is read and stays
/// until it is dismissed.
fn overturn(item: &ItemRef, window: &mut Window, cx: &mut App) -> ActionOutcome {
    match locate(item, cx) {
        Ok(Located::Path { path, line }) => {
            open_in_editor(&item.workspace, path, line, false, window, cx);
            ActionOutcome::MarkRead
        }
        Ok(_) => {
            bongos_minibuffer::message(window, cx, "overturn needs a file as the target");
            ActionOutcome::Keep
        }
        Err(message) => {
            bongos_minibuffer::message(window, cx, message);
            ActionOutcome::Keep
        }
    }
}

/// `open-task`: focuses the task of a `task:<id>` target.
fn open_task(item: &ItemRef, window: &mut Window, cx: &mut App) -> ActionOutcome {
    match locate(item, cx) {
        Ok(Located::Task(id)) => {
            let focus = cx
                .try_global::<InboxActions>()
                .and_then(|actions| actions.focus_task.clone());
            match focus {
                Some(focus) => {
                    focus(&id, &item.workspace, window, cx);
                    ActionOutcome::MarkRead
                }
                None => {
                    bongos_minibuffer::message(window, cx, "tasks are not available here");
                    ActionOutcome::Keep
                }
            }
        }
        Ok(_) => {
            bongos_minibuffer::message(window, cx, "open-task needs a task: target");
            ActionOutcome::Keep
        }
        Err(message) => {
            bongos_minibuffer::message(window, cx, message);
            ActionOutcome::Keep
        }
    }
}

/// `open-run`: shows the run of a `run:<id>` target.
fn open_run(item: &ItemRef, window: &mut Window, cx: &mut App) -> ActionOutcome {
    match locate(item, cx) {
        Ok(Located::Run(_)) => {
            if let Ok(action) = cx.build_action("workflow::ShowRun", None) {
                window.dispatch_action(action, cx);
                ActionOutcome::MarkRead
            } else {
                bongos_minibuffer::message(window, cx, "runs are not available here");
                ActionOutcome::Keep
            }
        }
        Ok(_) => {
            bongos_minibuffer::message(window, cx, "open-run needs a run: target");
            ActionOutcome::Keep
        }
        Err(message) => {
            bongos_minibuffer::message(window, cx, message);
            ActionOutcome::Keep
        }
    }
}

fn gone_message(target: Option<&str>) -> String {
    format!("the target is gone: {}", target.unwrap_or_default())
}

/// Where the target of `item` is, or what to say instead.
fn locate(item: &ItemRef, cx: &App) -> Result<Located, String> {
    let Some(target) = item.target.as_deref() else {
        return Err("this item has no target".to_string());
    };
    if item.target_state == Some(TargetState::Stale) {
        return Err(gone_message(Some(target)));
    }
    let Some(inbox) = Inbox::global(cx) else {
        return Err("the inbox is off".to_string());
    };
    let own: Vec<PathBuf> = item
        .workspace
        .upgrade()
        .map(|workspace| {
            workspace
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        })
        .unwrap_or_default();
    match inbox.read(cx).resolvers().locate(target, &own, cx) {
        Located::Gone => Err(gone_message(Some(target))),
        Located::Unresolved(why) => Err(format!("cannot open {target}: {why}")),
        located => Ok(located),
    }
}

/// `o`: opens the target of `item`, whatever its form. Says why in the echo area when it cannot.
pub(crate) fn open_target(item: &ItemRef, window: &mut Window, cx: &mut App) -> ActionOutcome {
    match locate(item, cx) {
        Ok(Located::Path { path, line }) => {
            open_in_editor(&item.workspace, path, line, false, window, cx);
            ActionOutcome::MarkRead
        }
        Ok(Located::Task(_)) => open_task(item, window, cx),
        Ok(Located::Run(_)) => open_run(item, window, cx),
        Ok(Located::Other { scheme, .. }) => {
            bongos_minibuffer::message(window, cx, format!("cannot open {scheme}: targets"));
            ActionOutcome::Keep
        }
        Ok(Located::Gone | Located::Unresolved(_)) => ActionOutcome::Keep,
        Err(message) => {
            bongos_minibuffer::message(window, cx, message);
            ActionOutcome::Keep
        }
    }
}

/// Opens `path` in an editor tab of `workspace`, at `line` (1-based) when there is one. A
/// read-only tab is markdown highlighted.
pub(crate) fn open_in_editor(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    line: Option<u32>,
    read_only: bool,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let location =
                Workspace::project_path_for_path(workspace.project().clone(), &path, false, cx);
            cx.spawn_in(window, async move |workspace, cx| {
                let (worktree, project_path) = location.await?;
                // A file outside the project is a worktree of its own, and its entry is there
                // once the scan has finished.
                let scanned = worktree.read_with(cx, |worktree, _| {
                    worktree.as_local().map(|local| local.scan_complete())
                });
                if let Some(scanned) = scanned {
                    scanned.await;
                }
                let item = workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_path(project_path, None, true, window, cx)
                    })?
                    .await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let Some(editor) = item.downcast::<Editor>() else {
                        return;
                    };
                    if read_only {
                        editor.update(cx, |editor, _| editor.set_read_only(true));
                    }
                    if let Some(line) = line {
                        let point = Point::new(line.saturating_sub(1), 0);
                        editor.update(cx, |editor, cx| {
                            editor.go_to_singleton_buffer_point(point, window, cx)
                        });
                    }
                    let buffer = editor.read(cx).buffer().read(cx).as_singleton();
                    let languages = workspace.project().read(cx).languages().clone();
                    if let (true, Some(buffer)) = (read_only, buffer) {
                        cx.spawn(async move |_, cx| {
                            let language = languages.language_for_name("Markdown").await?;
                            buffer.update(cx, |buffer: &mut Buffer, cx| {
                                buffer.set_language(Some(language), cx)
                            });
                            anyhow::Ok(())
                        })
                        .detach_and_log_err(cx);
                    }
                })
            })
            .detach_and_log_err(cx);
        })
        .log_err();
}
