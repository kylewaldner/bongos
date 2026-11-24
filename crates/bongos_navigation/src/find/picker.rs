//! The find in files picker: a session whose rows are the matches of [`engine::search`] for
//! what is typed, previewed in the pane a confirm would open in, and opened with the match
//! selected.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

use bongos_emacs::mark;
use bongos_minibuffer::{
    HistoryStore, Outcome, Preview as SessionPreview, Session, SessionHandle, Source, Value,
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
use crate::find::export;
use crate::find::flag_history;
use crate::find::replace;
use crate::preview::{Preview, PreviewSession, PreviewTarget};
use crate::registry;
use crate::target::{self, PaneChoice};

/// The name the session is routed by (`minibuffer.enabled_for`) and its history.
pub const HOST: &str = "find_in_files";
/// The key context of the picker's session, under `Minibuffer`.
pub const KEY_CONTEXT: &str = "FindInFiles";
/// The name of the history of the input.
pub const HISTORY: &str = "find-in-files";

/// The prompt in front of the input: `Find ` and the lit flags, as `Find [.*][Aa][w][buf]: `.
pub fn prompt(flags: &Flags) -> String {
    prompt_for(flags, false)
}

/// The prompt, with `[replace]` after the flags in replace mode.
pub fn prompt_for(flags: &Flags, replace: bool) -> String {
    let mode = if replace { replace::PROMPT_MARK } else { "" };
    format!("Find {}{mode}: ", flags.prompt_marks())
}

/// How a picker starts.
#[derive(Default)]
pub struct Start {
    /// The input the picker opens with.
    pub input: String,
    /// Whether the input is selected, so that typing replaces it.
    pub select_input: bool,
    pub flags: Flags,
    /// The directory every search stays in, as a project path.
    pub base_dir: Option<String>,
    /// Whether RET replaces the matches instead of opening one.
    pub replace: bool,
}

/// One open find in files picker.
pub struct FindPicker {
    workspace: WeakEntity<Workspace>,
    /// The editor that was active when the picker opened: a confirm pushes its mark and its
    /// place on the goto stack.
    origin: Option<WeakEntity<Editor>>,
    seed: String,
    base_dir: Option<String>,
    flags: RefCell<Flags>,
    /// Replace mode: RET replaces every match instead of opening one.
    replace: Cell<bool>,
    /// What the input holds.
    input: RefCell<String>,
    /// The buffers a search of the open buffers covers, collected when the flag is set.
    buffers: RefCell<Vec<Entity<Buffer>>>,
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
    /// Whether the picker is in replace mode: RET asks for a replacement of every match.
    pub fn replace_mode(&self) -> bool {
        self.replace.get()
    }

    /// Switches replace mode and shows it in the prompt.
    pub fn set_replace_mode(&self, replace: bool, window: &mut Window, cx: &mut App) {
        self.replace.set(replace);
        if let Some(handle) = self.handle() {
            handle.set_prompt(prompt_for(&self.flags.borrow(), replace), window, cx);
        }
    }

    /// The input the picker opened with.
    pub fn seed(&self) -> &str {
        &self.seed
    }

    /// The directory every search of this picker stays in, as a project path.
    pub fn base_dir(&self) -> Option<&str> {
        self.base_dir.as_deref()
    }

    /// The flags the searches run with.
    pub fn flags(&self) -> Flags {
        self.flags.borrow().clone()
    }

    /// What the input holds.
    pub fn input(&self) -> String {
        self.input.borrow().clone()
    }

    /// What the picker searches for now: its input, flags, scope and open buffers.
    pub(crate) fn request(&self) -> FindRequest {
        let mut request = FindRequest::new(&self.input.borrow(), self.flags())
            .buffers(self.buffers.borrow().clone());
        if let Some(dir) = &self.base_dir {
            request = request.base_dir(dir.clone());
        }
        request
    }

    /// Replaces the flags, shows them in the prompt and searches again. With the open buffers
    /// flag set, `buffers` is the list the search covers.
    pub fn set_flags(
        &self,
        flags: Flags,
        buffers: Vec<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let prompt = prompt_for(&flags, self.replace.get());
        self.flags.replace(flags);
        self.buffers.replace(buffers);
        if let Some(handle) = self.handle() {
            handle.set_prompt(prompt, window, cx);
            handle.refresh(window, cx);
        }
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

    /// Keeps the flags of the input that was confirmed, beside its history entry.
    fn remember_flags(&self, cx: &mut App) {
        let kvp = HistoryStore::global(cx).kvp().clone();
        flag_history::write(&kvp, &self.input.borrow(), &self.flags.borrow(), cx);
    }

    /// Takes the flags `entry` was run with, when they were kept.
    fn restore_flags(&self, entry: &str, window: &mut Window, cx: &mut App) {
        let kvp = HistoryStore::global(cx).kvp().clone();
        let Some(flags) = flag_history::read(&kvp, entry) else {
            return;
        };
        let buffers = match self.workspace.upgrade() {
            Some(workspace) if flags.open_buffers => open_buffers(workspace.read(cx), cx),
            _ => Vec::new(),
        };
        self.set_flags(flags, buffers, window, cx);
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

    /// Opens every match of the current query in a results multibuffer.
    fn export(&self, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            export::open(self, workspace, window, cx)
        });
    }

    /// Asks for the replacement of every match and opens the decision view on the first one.
    fn replace(&self, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            replace::ask(self, false, workspace, window, cx)
        });
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

/// The buffers of every editor open in the panes.
pub fn open_buffers(workspace: &Workspace, cx: &App) -> Vec<Entity<Buffer>> {
    let mut buffers: Vec<Entity<Buffer>> = Vec::new();
    for editor in workspace.items_of_type::<Editor>(cx) {
        for buffer in editor.read(cx).buffer().read(cx).all_buffers() {
            if !buffers.contains(&buffer) {
                buffers.push(buffer);
            }
        }
    }
    buffers
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
    let input = seed(editor.as_ref(), window, cx);
    let base_dir = scoped
        .then(|| file_directory(editor.as_ref(), workspace, cx))
        .flatten();
    let start = Start {
        select_input: true,
        input,
        base_dir,
        ..Start::default()
    };
    open_with(workspace, start, window, cx)
}

/// Opens the picker as `start` says.
pub fn open_with(
    workspace: &mut Workspace,
    start: Start,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Rc<FindPicker> {
    let editor = workspace.active_item_as::<Editor>(cx);
    let Start {
        input: seed,
        select_input,
        flags,
        base_dir,
        replace,
    } = start;
    let buffers = if flags.open_buffers {
        open_buffers(workspace, cx)
    } else {
        Vec::new()
    };
    let prompt = prompt_for(&flags, replace);
    let this = cx.entity();
    let picker = Rc::new(FindPicker {
        workspace: this.downgrade(),
        origin: editor.as_ref().map(Entity::downgrade),
        seed: seed.clone(),
        base_dir: base_dir.clone(),
        flags: RefCell::new(flags),
        replace: Cell::new(replace),
        input: RefCell::new(seed.clone()),
        buffers: RefCell::new(buffers),
        preview: RefCell::new(None),
        handle: RefCell::new(None),
    });

    let project = workspace.project().clone();
    let source = {
        let picker = Rc::downgrade(&picker);
        move |query: String, cancel, cx: &mut App| {
            let (flags, buffers) = picker.upgrade().map_or_else(Default::default, |picker| {
                (picker.flags(), picker.buffers.borrow().clone())
            });
            let mut request = FindRequest::new(&query, flags).buffers(buffers);
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
    let mut session = Session::completing_read(HOST, prompt)
        .history(HISTORY)
        .key_context(KEY_CONTEXT)
        .source(Source::Dynamic(Box::new(source)))
        .initial_input(seed);
    if select_input {
        session = session.select_initial_input();
    }
    let session = session
        .on_input({
            let picker = Rc::downgrade(&picker);
            move |text, _, _, _| {
                if let Some(picker) = picker.upgrade() {
                    picker.input.replace(text.to_string());
                }
            }
        })
        .on_history_entry({
            let picker = Rc::downgrade(&picker);
            move |entry, _, window, cx| {
                if let Some(picker) = picker.upgrade() {
                    picker.restore_flags(entry, window, cx);
                }
            }
        })
        .on_preview({
            let picker = picker.clone();
            move |event, window, cx| picker.on_preview(event, window, cx)
        })
        .on_confirm({
            let picker = picker.clone();
            move |outcome, window, cx| {
                picker.remember_flags(cx);
                if picker.replace_mode() {
                    picker.replace(window, cx);
                } else if let Outcome::Candidate(value) = outcome {
                    if matches!(value.downcast_ref::<FindRow>(), Some(FindRow::More { .. })) {
                        picker.export(window, cx);
                    } else {
                        picker.open_row(value, None, window, cx);
                    }
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
pub(crate) mod tests;
