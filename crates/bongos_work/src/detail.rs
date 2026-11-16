//! The details of one task in a tab (`task: show details`): its goal, kind and state, documents,
//! todos, children and edges, then one section for each source registered in
//! [`TaskSectionsRegistry`], lower order first. [`lines`] is pure; the tab, `task: dump details`
//! and the tests all read the lines it returns. With no source registered only the task's own
//! parts appear.

use std::rc::Rc;

use bongos_work_model::TodoState;
use bongos_work_model::Visibility;
use bongos_work_model::artifacts::task_artifacts;
use bongos_work_model::commands::kind_name;
use bongos_work_model::derive;
use bongos_work_model::graph::state_label;
use bongos_work_model::store::{Task, WorkTree};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    Styled as _, Subscription, UniformListScrollHandle, Window, div, uniform_list,
};
use ui::{Color, Icon, IconName, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::actions::{task, task_detail};
use crate::commands::{ready, show_toast, task_choices};
use crate::{WorkStore, WorkStoreEvent, modals, task_at_point};

/// How a row of a section reads: a status color, never a fixed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionTone {
    Success,
    Error,
    Warning,
    Muted,
    Neutral,
}

/// One row of a section. `id` names it to the source when it is activated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionRow {
    pub id: String,
    pub text: String,
    pub tone: SectionTone,
    /// Further text under the row.
    pub detail: Option<String>,
}

/// What a source adds to the details of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSection {
    pub title: String,
    pub header: String,
    pub rows: Vec<SectionRow>,
}

/// Something that adds a section to the details of a task and a line to its expanded row.
pub trait TaskSectionSource: 'static {
    /// The section for `task_id`, or `None` when the source has nothing to say. `show_sources`
    /// is what `task_detail: toggle sources` set in the view.
    fn section(&self, task_id: &str, show_sources: bool, cx: &App) -> Option<TaskSection>;
    /// One line for the expanded row of the task in the Work panel.
    fn summary(&self, task_id: &str, cx: &App) -> Option<String>;
    /// RET on the row `row_id` of this source's section.
    fn activate(&self, task_id: &str, row_id: &str, window: &mut Window, cx: &mut App);
}

#[derive(Default)]
struct TaskSectionSources(Vec<(String, i32, Rc<dyn TaskSectionSource>)>);

impl Global for TaskSectionSources {}

/// The sources of task sections. A namespace for the global registry.
pub struct TaskSectionsRegistry;

impl TaskSectionsRegistry {
    /// Adds the source `name` with sort key `order`, replacing one of the same name, and
    /// notifies the store.
    pub fn add_source(name: &str, order: i32, source: Rc<dyn TaskSectionSource>, cx: &mut App) {
        let sources = cx.default_global::<TaskSectionSources>();
        sources.0.retain(|(existing, ..)| existing != name);
        sources.0.push((name.to_string(), order, source));
        sources.0.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        Self::changed(cx);
    }

    /// Removes the source `name` and notifies the store.
    pub fn remove_source(name: &str, cx: &mut App) {
        cx.default_global::<TaskSectionSources>()
            .0
            .retain(|(existing, ..)| existing != name);
        Self::changed(cx);
    }

    /// Tells the store, and so every open view and the panel, that what a source reports changed.
    pub fn changed(cx: &mut App) {
        if let Some(store) = WorkStore::global(cx) {
            store.update(cx, |store, cx| store.live_modes_changed(cx));
        }
    }

    fn sources(cx: &App) -> Vec<(String, Rc<dyn TaskSectionSource>)> {
        cx.try_global::<TaskSectionSources>()
            .map(|sources| {
                sources
                    .0
                    .iter()
                    .map(|(name, _, source)| (name.clone(), source.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The sections of `task_id` with the name of the source of each, ordered by (order, name).
    pub fn sections(task_id: &str, show_sources: bool, cx: &App) -> Vec<(String, TaskSection)> {
        Self::sources(cx)
            .into_iter()
            .filter_map(|(name, source)| Some((name, source.section(task_id, show_sources, cx)?)))
            .collect()
    }

    /// The summary lines of `task_id`, in the order of the sources.
    pub fn summaries(task_id: &str, cx: &App) -> Vec<String> {
        Self::sources(cx)
            .into_iter()
            .filter_map(|(_, source)| source.summary(task_id, cx))
            .collect()
    }

    /// Hands RET on a row to the source `name`.
    pub fn activate(name: &str, task_id: &str, row_id: &str, window: &mut Window, cx: &mut App) {
        let source = Self::sources(cx)
            .into_iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, source)| source);
        if let Some(source) = source {
            source.activate(task_id, row_id, window, cx);
        }
    }
}

/// What a line stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineKind {
    Heading,
    Text,
    /// A child task; RET opens its details.
    Child(String),
    /// The title of the section of a source.
    SectionHeader(String),
    /// A row of the section of a source; RET activates it.
    SectionRow {
        source: String,
        row_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailLine {
    pub kind: LineKind,
    pub text: String,
    pub tone: SectionTone,
    /// Levels of indent.
    pub indent: usize,
}

impl DetailLine {
    fn new(kind: LineKind, text: impl Into<String>, indent: usize) -> Self {
        Self {
            kind,
            text: text.into(),
            tone: SectionTone::Neutral,
            indent,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self.kind, LineKind::Heading | LineKind::Text)
    }
}

fn todo_mark(state: TodoState) -> &'static str {
    match state {
        TodoState::Open => "[ ]",
        TodoState::Done => "[x]",
        TodoState::Skipped => "[-]",
        TodoState::Cancelled => "[~]",
    }
}

/// The lines of the details of `task`: its own parts, then `sections` as given (they are
/// already ordered).
pub fn lines(task: &Task, tree: &WorkTree, sections: &[(String, TaskSection)]) -> Vec<DetailLine> {
    let mut out = Vec::new();
    let heading = |out: &mut Vec<DetailLine>, text: String| {
        out.push(DetailLine::new(LineKind::Heading, text, 0))
    };
    let text = |out: &mut Vec<DetailLine>, text: String| {
        out.push(DetailLine::new(LineKind::Text, text, 1))
    };
    out.push(DetailLine::new(
        LineKind::Heading,
        format!("Goal: {}", task.goal()),
        0,
    ));
    if let Some(file) = &task.file {
        text(&mut out, format!("Kind: {}", kind_name(file.kind)));
    }
    let state = derive::state(task, tree);
    let mut state_text = format!("State: {}", state_label(state));
    if let Some(hold) = &task.done_hold {
        state_text.push_str(&format!(" (held from Done: {hold})"));
    }
    text(&mut out, state_text);

    let artifacts = task_artifacts(task).artifacts;
    let visibility = |path: &str| match artifacts
        .iter()
        .find(|artifact| artifact.path == path)
        .and_then(|artifact| artifact.visibility)
    {
        Some(Visibility::HumanOnly) => "human-only",
        _ => "shared",
    };
    let documents: Vec<String> = task
        .files
        .keys()
        .filter(|path| path.starts_with("documents"))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect();
    heading(&mut out, format!("Documents ({})", documents.len()));
    for path in &documents {
        text(&mut out, format!("{path}  {}", visibility(path)));
    }
    let others: Vec<_> = artifacts
        .iter()
        .filter(|artifact| !artifact.path.starts_with("documents/"))
        .collect();
    if !others.is_empty() {
        heading(&mut out, format!("Artifacts ({})", others.len()));
        for artifact in others {
            let mut line = format!("{}  {}", artifact.path, visibility(&artifact.path));
            if !artifact.description.is_empty() {
                line.push_str(&format!("  {}", artifact.description));
            }
            text(&mut out, line);
        }
    }

    if let Some(file) = &task.file {
        heading(&mut out, format!("Todos ({})", file.todos.len()));
        for todo in &file.todos {
            let mut line = format!("{} {}", todo_mark(todo.state), todo.name);
            if let Some(workflow) = &todo.workflow {
                line.push_str(&format!("  workflow {workflow}"));
            }
            text(&mut out, line);
        }
    }

    heading(&mut out, format!("Children ({})", task.children.len()));
    for child in &task.children {
        let line = format!(
            "{}  [{}]",
            child.named_goal(),
            state_label(derive::state(child, tree))
        );
        match child.id() {
            Some(id) => out.push(DetailLine::new(LineKind::Child(id.to_string()), line, 1)),
            None => text(&mut out, line),
        }
    }

    let mut edges = Vec::new();
    if let Some(file) = &task.file {
        edges.extend(file.after.iter().map(|edge| format!("after {}", edge.task)));
        if let Some(stacked) = &file.stacked_on {
            edges.push(format!("stacked on {}", stacked.task));
        }
    }
    edges.extend(
        task.broken_edges
            .iter()
            .map(|target| format!("broken edge: {target} is not a sibling")),
    );
    heading(&mut out, format!("Edges ({})", edges.len()));
    for edge in edges {
        text(&mut out, edge);
    }

    for (source, section) in sections {
        out.push(DetailLine::new(
            LineKind::SectionHeader(source.clone()),
            section.title.clone(),
            0,
        ));
        text(&mut out, section.header.clone());
        for row in &section.rows {
            let mut line = DetailLine::new(
                LineKind::SectionRow {
                    source: source.clone(),
                    row_id: row.id.clone(),
                },
                row.text.clone(),
                1,
            );
            line.tone = row.tone;
            out.push(line);
            if let Some(detail) = &row.detail {
                for part in detail.lines() {
                    let mut extra = DetailLine::new(LineKind::Text, part, 2);
                    extra.tone = SectionTone::Muted;
                    out.push(extra);
                }
            }
        }
    }
    out
}

/// The lines as plain text, one per line, for the dump and the tests.
pub(crate) fn plain(lines: &[DetailLine]) -> String {
    lines
        .iter()
        .map(|line| format!("{}{}\n", "  ".repeat(line.indent), line.text))
        .collect()
}

/// Which line a view opens with selected: the section titled `title`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionFocus {
    pub title: String,
}

/// The details of one task as a workspace item.
pub struct TaskDetailView {
    task_id: String,
    title: String,
    lines: Vec<DetailLine>,
    selected: Option<usize>,
    show_sources: bool,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<()> for TaskDetailView {}

impl TaskDetailView {
    fn new(task_id: String, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(store) = WorkStore::global(cx) {
            subscriptions
                .push(cx.subscribe(&store, |this, _, _: &WorkStoreEvent, cx| this.rebuild(cx)));
        }
        let mut this = Self {
            task_id,
            title: String::new(),
            lines: Vec::new(),
            selected: None,
            show_sources: false,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn lines(&self) -> &[DetailLine] {
        &self.lines
    }

    pub fn selected_line(&self) -> Option<&DetailLine> {
        self.lines.get(self.selected?)
    }

    pub fn show_sources(&self) -> bool {
        self.show_sources
    }

    /// Reads the store and the sources again, keeping the selection when its line is still there.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        let kept = self.selected_line().map(|line| line.kind.clone());
        let app: &App = cx;
        let built = WorkStore::global(app)
            .and_then(|store| store.read(app).snapshot())
            .and_then(|tree| {
                let task = tree.task_by_id(&self.task_id)?;
                let sections =
                    TaskSectionsRegistry::sections(&self.task_id, self.show_sources, app);
                Some((task.goal().to_string(), lines(task, &tree, &sections)))
            });
        match built {
            Some((goal, lines)) => {
                self.title = goal;
                self.lines = lines;
            }
            None => {
                self.lines = vec![DetailLine::new(
                    LineKind::Heading,
                    "The task is not in the Work tree",
                    0,
                )];
            }
        }
        self.selected = kept
            .and_then(|kind| self.lines.iter().position(|line| line.kind == kind))
            .or_else(|| self.lines.iter().position(DetailLine::selectable));
        self.scroll_to_selected();
        cx.notify();
    }

    fn focus_section(&mut self, focus: &SectionFocus) {
        let at = self.lines.iter().position(|line| {
            matches!(line.kind, LineKind::SectionHeader(_)) && line.text == focus.title
        });
        if at.is_some() {
            self.selected = at;
            self.scroll_to_selected();
        }
    }

    fn scroll_to_selected(&self) {
        if let Some(at) = self.selected {
            self.scroll_handle
                .scroll_to_item(at, ScrollStrategy::Center);
        }
    }

    fn select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let rows: Vec<usize> = (0..self.lines.len())
            .filter(|&at| self.lines[at].selectable())
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|at| rows.iter().position(|&row| row == at));
        let next = match current {
            Some(current) => (current as isize + offset).clamp(0, rows.len() as isize - 1) as usize,
            None if offset < 0 => rows.len() - 1,
            None => 0,
        };
        self.selected = Some(rows[next]);
        self.scroll_to_selected();
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = self.selected_line().cloned() else {
            return;
        };
        match line.kind {
            LineKind::Child(id) => {
                window.defer(cx, move |window, cx| {
                    if let Some(workspace) = window.root::<workspace::MultiWorkspace>().flatten() {
                        workspace
                            .read(cx)
                            .workspace()
                            .clone()
                            .update(cx, |workspace, cx| {
                                show_details(&id, None, workspace, window, cx)
                            });
                    }
                });
            }
            LineKind::SectionRow { source, row_id } => {
                let task_id = self.task_id.clone();
                window.defer(cx, move |window, cx| {
                    TaskSectionsRegistry::activate(&source, &task_id, &row_id, window, cx)
                });
            }
            _ => {}
        }
    }

    fn render_line(&self, index: usize, line: &DetailLine, cx: &App) -> AnyElement {
        let color = match line.tone {
            SectionTone::Success => Color::Success,
            SectionTone::Error => Color::Error,
            SectionTone::Warning => Color::Warning,
            SectionTone::Muted => Color::Muted,
            SectionTone::Neutral => Color::Default,
        };
        let size = match line.kind {
            LineKind::Heading | LineKind::SectionHeader(_) => LabelSize::Default,
            _ => LabelSize::Small,
        };
        let selected = self.selected == Some(index);
        let mut row = div()
            .id(("task-detail-line", index))
            .w_full()
            .px_2()
            .pl(rems(0.5 + 1.0 * line.indent as f32))
            .child(Label::new(line.text.clone()).size(size).color(color));
        if selected {
            row = row.bg(cx.theme().colors().element_selected);
        }
        row.into_any_element()
    }
}

impl Render for TaskDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("task-detail-view")
            .key_context("TaskDetailView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.select_offset(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.select_offset(-1, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .on_action(cx.listener(|this, _: &task_detail::Refresh, _, cx| this.rebuild(cx)))
            .on_action(cx.listener(|this, _: &task_detail::ToggleSources, _, cx| {
                this.show_sources = !this.show_sources;
                this.rebuild(cx);
            }))
            .size_full()
            .child(
                uniform_list(
                    "task-detail-lines",
                    self.lines.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .filter_map(|index| {
                                Some(this.render_line(index, this.lines.get(index)?, cx))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .size_full(),
            )
    }
}

impl Focusable for TaskDetailView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TaskDetailView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Task: {}", self.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// Shows the details of `task_id`: the open view of that task when there is one, else a new item
/// in the active pane. `focus` selects the line of a section.
pub fn show_details(
    task_id: &str,
    focus: Option<SectionFocus>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_details(task_id, focus, false, workspace, window, cx)
}

/// Like [`show_details`], with the sources of every section shown, as `task_detail: toggle
/// sources` shows them.
pub fn show_details_with_sources(
    task_id: &str,
    focus: Option<SectionFocus>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_details(task_id, focus, true, workspace, window, cx)
}

fn open_details(
    task_id: &str,
    focus: Option<SectionFocus>,
    sources: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<TaskDetailView>(cx)
        .find(|view| view.read(cx).task_id == task_id);
    let view: Entity<TaskDetailView> = match existing {
        Some(view) => {
            view.update(cx, |view, cx| {
                view.show_sources |= sources;
                view.rebuild(cx)
            });
            workspace.activate_item(&view, true, true, window, cx);
            view
        }
        None => {
            let view = cx.new(|cx| {
                let mut view = TaskDetailView::new(task_id.to_string(), cx);
                if sources {
                    view.show_sources = true;
                    view.rebuild(cx);
                }
                view
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        }
    };
    if let Some(focus) = focus {
        view.update(cx, |view, cx| {
            view.focus_section(&focus);
            cx.notify();
        });
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|w, _: &task::ShowDetails, window, cx| {
        with_details_task(w, window, cx, |id, w, window, cx| {
            show_details(&id, None, w, window, cx)
        })
    });
    #[cfg(debug_assertions)]
    workspace.register_action(|w, _: &task::DumpDetails, window, cx| {
        with_details_task(w, window, cx, |id, w, _, cx| dump_details(&id, w, cx))
    });
}

/// Runs `then` on the task at point, else the focused task, else one picked.
fn with_details_task(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(String, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    if let Some(id) = task_at_point(workspace, window, cx) {
        then(id, workspace, window, cx);
        return;
    }
    let (ids, labels): (Vec<String>, Vec<String>) =
        task_choices(&tree, |_, _| true).into_iter().unzip();
    if ids.is_empty() {
        show_toast(workspace, "There are no tasks".to_string(), cx);
        return;
    }
    modals::pick(
        workspace,
        "Task",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            then(ids[index].clone(), workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// The file `task: dump details` writes in the logs directory.
#[cfg(debug_assertions)]
pub(crate) const DUMP_FILE: &str = "task-details.txt";

/// The plain text of the details of `task_id`, as the view shows them.
pub(crate) fn text_of(task_id: &str, show_sources: bool, cx: &App) -> Option<String> {
    let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
    let task = tree.task_by_id(task_id)?;
    let sections = TaskSectionsRegistry::sections(task_id, show_sources, cx);
    Some(plain(&lines(task, &tree, &sections)))
}

#[cfg(debug_assertions)]
fn dump_details(task_id: &str, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(text) = text_of(task_id, false, cx) else {
        show_toast(
            workspace,
            "The task is not in the Work tree".to_string(),
            cx,
        );
        return;
    };
    let logs_dir = paths::logs_dir();
    let written = std::fs::create_dir_all(logs_dir)
        .and_then(|()| std::fs::write(logs_dir.join(DUMP_FILE), &text));
    let message = match written {
        Ok(()) => format!("Wrote {}", logs_dir.join(DUMP_FILE).display()),
        Err(error) => format!("Could not write the details: {error}"),
    };
    show_toast(workspace, message, cx);
}
