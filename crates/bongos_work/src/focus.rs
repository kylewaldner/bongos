//! `task: focus`, `task: unfocus` and `project: focus` (D21): the window switches to the focused
//! task's worktree, or to its directory for a group or a leaf with no worktree, without saving or
//! closing anything. The workspace left behind stays alive in the same window (a `MultiWorkspace`)
//! with its unsaved buffers, and focusing back reactivates it.
//!
//! The window's project is never replaced: that path runs `Workspace::prepare_to_close`, which
//! prompts to save dirty buffers. The focused task id is kept in the key value store under
//! `bongos_work` / `focus` (D20) and restored at startup; an id that no longer names a task is
//! dropped silently.

use std::path::PathBuf;

use anyhow::Result;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, Global, Subscription, WeakEntity,
    Window, WindowHandle,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, PathList, Workspace};

use crate::actions::{project, task};
use crate::commands::{ready, show_toast, task_choices, with_project};
use crate::worktrees::expand_home;
use crate::{WorkStore, WorkStoreEvent, modals};

/// The key value store namespace of the Work view's UI state (D20).
pub const KVP_NAMESPACE: &str = "bongos_work";
/// The key of the focused task id, a JSON string.
pub const FOCUS_KEY: &str = "focus";

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &task::Focus, window, cx| focus(w, window, cx))
        .register_action(|w, _: &task::Unfocus, window, cx| unfocus(w, window, cx))
        .register_action(|w, _: &project::Focus, window, cx| focus_project(w, window, cx));
}

/// The focused task, shared by every window.
pub struct Focus {
    task: Option<String>,
    /// The workspace that was active when focus first left a workspace it did not open;
    /// `task: unfocus` goes back to it.
    home: Option<WeakEntity<Workspace>>,
    /// The workspaces focus opened or switched to.
    opened: Vec<WeakEntity<Workspace>>,
    _store: Option<Subscription>,
}

struct GlobalFocus(Entity<Focus>);

impl Global for GlobalFocus {}

/// Creates the global [`Focus`] with the task id read from the key value store. Call after the
/// global [`WorkStore`] exists, so an id that no longer names a task is dropped when it loads.
pub fn init(cx: &mut App) {
    let task = read_persisted(cx);
    let store = WorkStore::global(cx);
    let tree = store.as_ref().and_then(|store| store.read(cx).snapshot());
    let focus = cx.new(|cx| {
        let mut focus = Focus {
            task,
            home: None,
            opened: Vec::new(),
            _store: store.map(|store| cx.subscribe(&store, Focus::store_changed)),
        };
        if let Some(tree) = tree {
            focus.drop_unknown(&tree, cx);
        }
        focus
    });
    cx.set_global(GlobalFocus(focus));
}

impl Focus {
    /// The global focus, once [`init`] has run.
    pub fn global(cx: &App) -> Option<Entity<Focus>> {
        cx.try_global::<GlobalFocus>()
            .map(|global| global.0.clone())
    }

    /// The id of the focused task.
    pub fn task_id(&self) -> Option<&str> {
        self.task.as_deref()
    }

    /// Focuses the task `task` (or none) without opening anything.
    pub fn set(&mut self, task: Option<String>, cx: &mut Context<Self>) {
        if self.task == task {
            return;
        }
        self.task = task.clone();
        persist(task, cx);
        cx.notify();
    }

    fn store_changed(
        &mut self,
        store: Entity<WorkStore>,
        event: &WorkStoreEvent,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, WorkStoreEvent::Changed) {
            return;
        }
        if let Some(tree) = store.read(cx).snapshot() {
            self.drop_unknown(&tree, cx);
        }
    }

    /// Clears the focus when its id names no task in `tree`.
    fn drop_unknown(&mut self, tree: &WorkTree, cx: &mut Context<Self>) {
        if let Some(id) = &self.task
            && tree.task_by_id(id).is_none()
        {
            self.set(None, cx);
        }
    }

    /// Focuses the task `id`, leaving the workspace `leaving`.
    fn focus(&mut self, id: String, leaving: WeakEntity<Workspace>, cx: &mut Context<Self>) {
        self.opened
            .retain(|workspace| workspace.upgrade().is_some());
        if !self.opened.contains(&leaving) {
            self.home = Some(leaving);
        }
        self.set(Some(id), cx);
    }

    fn opened(&mut self, workspace: &Entity<Workspace>) {
        let workspace = workspace.downgrade();
        if self.home.as_ref() != Some(&workspace) && !self.opened.contains(&workspace) {
            self.opened.push(workspace);
        }
    }

    /// Clears the focus. Returns the workspace to go back to, when `current` is one focus opened
    /// and the workspace focus left is still alive.
    fn unfocus(
        &mut self,
        current: &WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Result<Option<Entity<Workspace>>, String> {
        if self.task.is_none() {
            return Err("No task is focused".to_string());
        }
        self.set(None, cx);
        let home = self.home.take().and_then(|home| home.upgrade());
        Ok(home.filter(|home| self.opened.contains(current) && home.downgrade() != *current))
    }
}

fn read_persisted(cx: &App) -> Option<String> {
    let value = KeyValueStore::global(cx)
        .scoped(KVP_NAMESPACE)
        .read(FOCUS_KEY)
        .log_err()
        .flatten()?;
    serde_json::from_str(&value).log_err()
}

fn persist(task: Option<String>, cx: &App) {
    let db = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        let store = db.scoped(KVP_NAMESPACE);
        match task {
            Some(id) => {
                store
                    .write(FOCUS_KEY.to_string(), serde_json::to_string(&id)?)
                    .await
            }
            None => store.delete(FOCUS_KEY.to_string()).await,
        }
    });
}

/// Where focusing a task takes the window.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    /// Why the window goes somewhere other than the task asks for.
    pub note: Option<String>,
}

/// The directory focusing the task `id` opens: a leaf's worktree, else the task directory.
pub fn target(tree: &WorkTree, id: &str) -> Result<Target, String> {
    let place = Place::locate(tree, id)?;
    let task = place.task;
    let at_dir = |note| Target {
        path: task.dir.clone(),
        note,
    };
    if derive::shape(task, tree) == Shape::Group {
        return Ok(at_dir(None));
    }
    let Some(worktree) = task.file.as_ref().and_then(|file| file.worktree.as_deref()) else {
        return Ok(at_dir(None));
    };
    let path = expand_home(worktree);
    if path.is_dir() {
        Ok(Target { path, note: None })
    } else {
        Ok(at_dir(Some(format!(
            "The worktree of {} is gone ({worktree}); focused its directory instead",
            place.slug()
        ))))
    }
}

/// Whether `id` is a project's top task: focusing a project focuses this task, and a chat on it is
/// a project chat.
pub fn is_project_top(tree: &WorkTree, id: &str) -> bool {
    tree.projects
        .iter()
        .any(|project| project.task.id() == Some(id))
}

fn focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer(crate::registry::view_opened);
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let (ids, labels): (Vec<String>, Vec<String>) =
        task_choices(&tree, |_, _| true).into_iter().unzip();
    if ids.is_empty() {
        show_toast(workspace, "There is no task to focus".to_string(), cx);
        return;
    }
    let focused = Focus::global(cx).and_then(|focus| focus.read(cx).task.clone());
    let selected = focused
        .and_then(|focused| ids.iter().position(|id| *id == focused))
        .unwrap_or(0);
    modals::pick(
        workspace,
        "Focus which task?",
        labels,
        selected,
        Box::new(move |index, workspace, window, cx| {
            focus_task(&ids[index], workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn focus_project(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_project(
        workspace,
        window,
        cx,
        "Focus which project?",
        "There is no project",
        |_| true,
        Box::new(|slug, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let id = tree
                .projects
                .iter()
                .find(|project| project.slug == slug)
                .and_then(|project| project.task.id().map(str::to_string));
            match id {
                Some(id) => focus_task(&id, workspace, window, cx),
                None => show_toast(workspace, format!("{slug} has no task.toml"), cx),
            }
        }),
    );
}

/// What runs in the workspace focus switched to, once it is active.
pub(crate) type AfterFocus =
    Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static>;

/// Focuses the task `id`: switches the window to its [`target`], and records the focus once that
/// has worked (a failed open leaves the focus as it was).
pub(crate) fn focus_task(
    id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    focus_task_then(id, None, workspace, window, cx)
}

/// [`focus_task`], then `after` in the workspace the window switched to.
pub(crate) fn focus_task_then(
    id: &str,
    after: Option<AfterFocus>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let Target { path, note } = match target(&tree, id) {
        Ok(target) => target,
        Err(message) => {
            show_toast(workspace, message, cx);
            return;
        }
    };
    let (Some(focus), Some(handle)) = (
        Focus::global(cx),
        window.window_handle().downcast::<MultiWorkspace>(),
    ) else {
        show_toast(
            workspace,
            "This window cannot switch workspaces".to_string(),
            cx,
        );
        return;
    };
    let leaving = cx.entity().downgrade();
    let id = id.to_string();
    cx.spawn_in(window, async move |workspace, cx| {
        match activate_paths(handle, vec![path], cx).await {
            Ok(opened) => {
                cx.update(|_, cx| {
                    focus.update(cx, |focus, cx| {
                        focus.focus(id, leaving, cx);
                        focus.opened(&opened);
                    });
                    if let Some(store) = WorkStore::global(cx) {
                        store.update(cx, |store, cx| store.refresh_rebase(cx));
                    }
                })
                .log_err();
                if let Some(note) = note {
                    opened.update(cx, |opened, cx| show_toast(opened, note, cx));
                }
                if let Some(after) = after {
                    opened
                        .update_in(cx, |opened, window, cx| after(opened, window, cx))
                        .log_err();
                }
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Could not focus the task: {error}"), cx)
                    })
                    .ok();
            }
        }
    })
    .detach();
}

/// Activates the workspace of `paths` in the window, creating it when the window has none.
///
/// The active workspace is retained first, so it stays alive when it is left even while the
/// window's multi-workspace sidebar is off (`activate` drops an unretained workspace then).
/// `open_project` is used when it cannot close anything: it closes an active workspace with no
/// folders (after a save prompt for its dirty untitled buffers), and without the sidebar it
/// replaces the project; in both cases the workspace is opened beside the active one instead.
async fn activate_paths(
    handle: WindowHandle<MultiWorkspace>,
    paths: Vec<PathBuf>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Workspace>> {
    let opening = handle.update(cx, |multi, window, cx| {
        multi.retain_active_workspace(cx);
        let has_folders = multi
            .workspace()
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .is_some();
        if multi.multi_workspace_enabled(cx) && has_folders {
            multi.open_project(paths, OpenMode::Activate, window, cx)
        } else {
            multi.find_or_create_local_workspace(
                PathList::new(&paths),
                None,
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        }
    })?;
    opening.await
}

fn unfocus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    let Some(focus) = Focus::global(cx) else {
        return;
    };
    let current = cx.entity().downgrade();
    let home = match focus.update(cx, |focus, cx| focus.unfocus(&current, cx)) {
        Ok(Some(home)) => home,
        Ok(None) => return,
        Err(message) => {
            show_toast(workspace, message, cx);
            return;
        }
    };
    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    cx.spawn_in(window, async move |_, cx| {
        handle
            .update(cx, |multi, window, cx| {
                if multi.workspaces().any(|workspace| *workspace == home) {
                    multi.retain_active_workspace(cx);
                    multi.activate(home, None, window, cx);
                }
            })
            .log_err();
    })
    .detach();
}
