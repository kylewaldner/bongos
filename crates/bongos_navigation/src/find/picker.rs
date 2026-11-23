//! The find in files picker: a session whose rows are the matches of [`engine::search`] for
//! what is typed, previewed in the pane a confirm would open in, and opened with the match
//! selected.

use std::{any::Any, cell::RefCell, ops::Range, rc::Rc};

use bongos_emacs::mark;
use bongos_minibuffer::{
    Outcome, Preview as SessionPreview, Session, SessionHandle, Source, Value,
};
use bongos_navigation_model::flags::Flags;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, Context, Entity, WeakEntity, Window};
use language::{Buffer, Point};
use project::ProjectPath;
use settings::SeedQuerySetting;
use text::{Anchor, ToPoint as _};
use workspace::{Pane, Workspace, searchable::SearchableItem as _};

use crate::find::engine::{self, FindRequest, FindRow};
use crate::preview::{Preview, PreviewSession, PreviewTarget};
use crate::registry;
use crate::target::{self, PaneChoice};

/// The name the session is routed by (`minibuffer.enabled_for`) and its history.
pub const HOST: &str = "find_in_files";
/// The prompt in front of the input.
pub const PROMPT: &str = "Find: ";

/// One open find in files picker.
pub struct FindPicker {
    workspace: WeakEntity<Workspace>,
    /// The editor that was active when the picker opened: a confirm pushes its mark and its
    /// place on the goto stack.
    origin: Option<WeakEntity<Editor>>,
    seed: String,
    base_dir: Option<String>,
    /// Made at the first preview: the workspace cannot be read while it opens the picker.
    preview: RefCell<Option<Entity<PreviewSession>>>,
    /// The open session; taken when it ends.
    handle: RefCell<Option<SessionHandle>>,
}

impl registry::SessionHandle for FindPicker {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FindPicker {
    /// The input the picker opened with.
    pub fn seed(&self) -> &str {
        &self.seed
    }

    /// The directory every search of this picker stays in, as a project path.
    pub fn base_dir(&self) -> Option<&str> {
        self.base_dir.as_deref()
    }

    /// The open session, until it ends.
    pub fn handle(&self) -> Option<SessionHandle> {
        self.handle.borrow().clone()
    }

    /// The preview session, once a row was previewed.
    pub fn preview(&self) -> Option<Entity<PreviewSession>> {
        self.preview.borrow().clone()
    }

    fn preview_session(&self, cx: &mut App) -> Option<Entity<PreviewSession>> {
        if let Some(preview) = self.preview() {
            return Some(preview);
        }
        let workspace = self.workspace.upgrade()?;
        let preview = cx.new(|cx| PreviewSession::new(&workspace, cx));
        self.preview.replace(Some(preview.clone()));
        Some(preview)
    }

    fn set_status(&self, status: engine::FindStatus, cx: &mut App) {
        if let Some(handle) = self.handle() {
            handle.set_status(status.label(), cx);
        }
    }

    fn on_preview(self: &Rc<Self>, event: SessionPreview, window: &mut Window, cx: &mut App) {
        let preview = match event {
            SessionPreview::Highlight(value) => self
                .preview_target(&value, cx)
                .map_or(Preview::NoCandidate, Preview::Highlight),
            SessionPreview::Confirmed => {
                self.end(cx);
                Preview::Confirmed
            }
            SessionPreview::Cancelled => {
                self.end(cx);
                Preview::Cancelled
            }
        };
        let session = match preview {
            Preview::Highlight(_) => self.preview_session(cx),
            _ => self.preview(),
        };
        if let Some(session) = session {
            session.update(cx, |session, cx| session.update(preview, window, cx));
        }
    }

    /// The match of `value` in the pane a confirm would open it in.
    fn preview_target(&self, value: &Value, cx: &App) -> Option<PreviewTarget> {
        let FindRow::Match { buffer, range, .. } = value.downcast_ref::<FindRow>()? else {
            return None;
        };
        let workspace = self.workspace.upgrade()?;
        let path = project_path(buffer, cx)?;
        let pane = target::resolve(workspace.read(cx), &PaneChoice::Default, cx).preview_pane;
        Some(PreviewTarget::Path {
            path,
            row: None,
            ranges: vec![point_range(buffer, range, cx)],
            pane,
        })
    }

    /// Forgets the session: it is no longer the workspace's open one.
    fn end(self: &Rc<Self>, cx: &mut App) {
        self.handle.borrow_mut().take();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let id = workspace.entity_id();
        let is_current = registry::current(cx, id)
            .is_some_and(|current| std::ptr::addr_eq(Rc::as_ptr(&current), Rc::as_ptr(self)));
        if is_current {
            registry::unregister(cx, id);
        }
    }

    /// Opens the match of `value` in `pane`, or in the default pane when `None`. The origin's
    /// mark and its place on the goto stack are pushed first. Any other row opens nothing.
    fn open_row(
        &self,
        value: Value,
        pane: Option<Entity<Pane>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(FindRow::Match { buffer, range, .. }) = value.downcast_ref::<FindRow>() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if let Some(origin) = self.origin.as_ref().and_then(WeakEntity::upgrade) {
            mark::push_mark(&origin, None, false, window, cx);
            origin.update(cx, |origin, cx| origin.push_goto_origin_at_cursor(cx));
        }
        let selection = point_range(buffer, range, cx);
        workspace.update(cx, |workspace, cx| {
            let pane = pane.unwrap_or_else(|| {
                let resolved = target::resolve(workspace, &PaneChoice::Default, cx);
                target::realize(workspace, resolved, window, cx)
            });
            // The origin's push is the one entry of this jump.
            pane.update(cx, |pane, _| pane.disable_history());
            let editor = workspace.open_project_item::<Editor>(
                pane.clone(),
                buffer.clone(),
                true,
                true,
                false,
                false,
                window,
                cx,
            );
            editor.update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([selection]),
                );
            });
            pane.update(cx, |pane, _| pane.enable_history());
        });
    }
}

fn project_path(buffer: &Entity<Buffer>, cx: &App) -> Option<ProjectPath> {
    let file = buffer.read(cx).file()?;
    Some(ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    })
}

fn point_range(buffer: &Entity<Buffer>, range: &Range<Anchor>, cx: &App) -> Range<Point> {
    let snapshot = buffer.read(cx).snapshot();
    range.start.to_point(&snapshot)..range.end.to_point(&snapshot)
}

/// What the picker starts with: the active editor's region, else the word at its cursor, else
/// nothing.
fn seed(editor: Option<&Entity<Editor>>, window: &mut Window, cx: &mut App) -> String {
    editor.map_or_else(String::new, |editor| {
        editor.update(cx, |editor, cx| {
            editor.query_suggestion(Some(SeedQuerySetting::Always), window, cx)
        })
    })
}

/// The directory of the active editor's file, as the project path the search is scoped to.
fn file_directory(
    editor: Option<&Entity<Editor>>,
    workspace: &Workspace,
    cx: &App,
) -> Option<String> {
    let buffer = editor?.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    let project = workspace.project().read(cx);
    // With several worktrees the rows and the globs carry the worktree's name.
    let path = if project.visible_worktrees(cx).nth(1).is_some() {
        file.full_path(cx).to_string_lossy().into_owned()
    } else {
        file.path().display(file.path_style(cx)).into_owned()
    };
    let dir = match path.rfind('/') {
        Some(ix) => &path[..ix],
        None => "",
    };
    Some(dir.to_string())
}

/// Opens the picker; `scoped` (a prefix argument) keeps the search in the active file's
/// directory.
pub fn open(
    workspace: &mut Workspace,
    scoped: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Rc<FindPicker> {
    let editor = workspace.active_item_as::<Editor>(cx);
    let seed = seed(editor.as_ref(), window, cx);
    let base_dir = scoped
        .then(|| file_directory(editor.as_ref(), workspace, cx))
        .flatten();
    let this = cx.entity();
    let picker = Rc::new(FindPicker {
        workspace: this.downgrade(),
        origin: editor.as_ref().map(Entity::downgrade),
        seed: seed.clone(),
        base_dir: base_dir.clone(),
        preview: RefCell::new(None),
        handle: RefCell::new(None),
    });

    let project = workspace.project().clone();
    let source = {
        let picker = Rc::downgrade(&picker);
        move |query: String, cancel, cx: &mut App| {
            let mut request = FindRequest::new(&query, Flags::default());
            if let Some(dir) = &base_dir {
                request = request.base_dir(dir.clone());
            }
            let picker = picker.clone();
            engine::search(
                &project,
                request,
                cancel,
                move |status, cx| {
                    if let Some(picker) = picker.upgrade() {
                        picker.set_status(status, cx);
                    }
                },
                cx,
            )
        }
    };
    let session = Session::completing_read(HOST, PROMPT)
        .source(Source::Dynamic(Box::new(source)))
        .initial_input(seed)
        .select_initial_input()
        .on_preview({
            let picker = picker.clone();
            move |event, window, cx| picker.on_preview(event, window, cx)
        })
        .on_confirm({
            let picker = picker.clone();
            move |outcome, window, cx| {
                if let Outcome::Candidate(value) = outcome {
                    picker.open_row(value, None, window, cx);
                }
            }
        });
    let session = target::add_pane_choices(session, this.downgrade(), {
        let picker = picker.clone();
        move |value, pane, window, cx| picker.open_row(value, Some(pane), window, cx)
    });
    let handle = bongos_minibuffer::open(workspace, session, window, cx);
    picker.handle.replace(Some(handle));
    registry::register(cx, this.entity_id(), picker.clone());
    picker
}

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;
