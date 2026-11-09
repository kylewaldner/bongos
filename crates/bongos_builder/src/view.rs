//! The proposal view (D12): one level of a parsed proposal in a workspace item, with a breadcrumb
//! above it and the problems of the file at the top. RET on a task row descends, `^` ascends, and
//! RET on a problem opens the raw file at its line.
//!
//! The text comes from the open buffer of the file when there is one, so edits made in the raw
//! file show at once, saved or not; otherwise it is read from disk on open and whenever the work
//! store changes. What the rows say is decided by [`analyze`] and [`build`], which have no
//! window or entity in them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bongos_builder_model::grammar::{kind_name, unblock_name};
use bongos_builder_model::validate::ProposalFile;
use bongos_builder_model::{
    Checks, Choices, Edge, LiveSet, Node, Op, Parsed, Problem, Severity, TaskRef, diff, parse,
    validate,
};
use bongos_work::WorkStore;
use bongos_work::model::Kind;
use bongos_work::model::derive::{self, State};
use bongos_work::model::rules::{EdgeKind, role_name};
use bongos_work::model::store::WorkTree;
use editor::Editor;
use fs::Fs;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, Styled as _,
    Subscription, Task, UniformListScrollHandle, WeakEntity, Window, div, uniform_list,
};
use language::{Buffer, BufferEvent, Point};
use project::buffer_store::BufferStoreEvent;
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::{
    Color, Icon, IconName, Label, LabelCommon as _, LabelSize, ListItem, ListItemSpacing, h_flex,
    prelude::*,
};
use workspace::item::{Item, ItemEvent};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::TaskBuilderSettings;
use crate::actions::show_toast;
use crate::choose::{self, Chosen};

/// What a task line of the proposal does to the tree, as the row says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    /// An existing task nothing is asked of.
    Untouched,
    /// An existing task that is renamed, retyped, re-todoed, linked or moved.
    Changed,
    /// Something the proposal asks of it is refused, so it stays as it is.
    Kept,
    /// An existing leaf that becomes a group.
    Split,
    /// An existing task that is cancelled.
    Cancel,
    /// A task line the tree does not have.
    New,
    /// A task that is done or cancelled: nothing is asked of it.
    Done,
}

impl Mark {
    pub fn word(self) -> &'static str {
        match self {
            Mark::Untouched => "untouched",
            Mark::Changed => "changed",
            Mark::Kept => "kept",
            Mark::Split => "split",
            Mark::Cancel => "cancel",
            Mark::New => "new",
            Mark::Done => "done",
        }
    }
}

/// What the tree and the diff say about a task line with an id.
#[derive(Clone, Copy, Debug)]
struct Fact {
    mark: Mark,
    kind: Kind,
}

/// A proposal text, parsed and checked against the tree.
#[derive(Clone, Debug)]
pub(crate) struct Analysis {
    pub parsed: Parsed,
    /// The parser's problems and validation's, in line order.
    pub problems: Vec<Problem>,
    /// By task id, for every task line whose id names a task in the tree.
    facts: HashMap<String, Fact>,
}

/// Parses `text`, validates it as proposal `number` of the task `task_id`, and diffs it against
/// `tree` for the marks. The running work is not collected here, so a split of a live leaf shows
/// as `split` until the apply asks (the preview step).
pub(crate) fn analyze(
    text: &str,
    number: u32,
    task_id: &str,
    tree: &WorkTree,
    todo_warning: u32,
) -> Analysis {
    let parsed = parse(text);
    let checks = Checks {
        workflows: None,
        todo_warning: todo_warning as usize,
        file: Some(ProposalFile {
            number,
            task_id: task_id.to_string(),
        }),
    };
    let mut problems = parsed.problems.clone();
    problems.extend(validate(&parsed, tree, &checks));
    problems.sort_by_key(|problem| problem.line);
    let diff = diff(&parsed, tree, &LiveSet::default(), &Choices::default());
    let mut ops: HashMap<&str, Mark> = HashMap::new();
    for op in &diff.ops {
        let (id, mark) = match op {
            Op::Cancel { id, .. } => (id.as_str(), Mark::Cancel),
            Op::Split { id, .. } => (id.as_str(), Mark::Split),
            Op::Kept { id, .. } => (id.as_str(), Mark::Kept),
            Op::Untouched { id } => (id.as_str(), Mark::Untouched),
            Op::AddEdge {
                from: TaskRef::Existing { id, .. },
                ..
            } => (id.as_str(), Mark::Changed),
            Op::Rename { id, .. }
            | Op::SetKind { id, .. }
            | Op::SetPlannedResult { id, .. }
            | Op::Todos { id, .. }
            | Op::RemoveEdge { id, .. }
            | Op::Move { id, .. }
            | Op::PlanDone { id, .. } => (id.as_str(), Mark::Changed),
            Op::Create { .. } | Op::AddEdge { .. } => continue,
        };
        let entry = ops.entry(id).or_insert(mark);
        *entry = (*entry).max(mark);
    }
    let mut facts = HashMap::new();
    collect_facts(&parsed.root, tree, &ops, &mut facts);
    Analysis {
        parsed,
        problems,
        facts,
    }
}

fn collect_facts(
    node: &Node,
    tree: &WorkTree,
    ops: &HashMap<&str, Mark>,
    facts: &mut HashMap<String, Fact>,
) {
    if let Some(id) = node.attributes.id.as_deref()
        && let Some(task) = tree.task_by_id(id)
        && let Some(file) = &task.file
    {
        let closed = matches!(derive::state(task, tree), State::Done | State::Cancelled);
        let mark = if closed {
            Mark::Done
        } else {
            ops.get(id).copied().unwrap_or(Mark::Untouched)
        };
        facts.insert(
            id.to_string(),
            Fact {
                mark,
                kind: file.kind,
            },
        );
    }
    for child in &node.children {
        collect_facts(child, tree, ops, facts);
    }
}

/// What a selectable line stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// The problem at this index of the list.
    Problem(usize),
    /// The task of the level with this label.
    Task(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// The problems line.
    Heading,
    Problem,
    Task,
    /// A result or todo of a leaf.
    Detail,
    /// An `after` or `stacked on` line of the task above.
    Edge,
}

#[derive(Clone, Debug)]
pub(crate) struct Line {
    pub kind: LineKind,
    pub text: String,
    /// The right side of a task line: its mark, kind and rollups.
    pub trailing: String,
    pub key: Option<Key>,
    /// Whether RET on the task descends: it has tasks below it.
    pub group: bool,
    /// For a problem: its 1-based line in the file.
    pub file_line: Option<usize>,
}

impl Line {
    fn new(kind: LineKind, text: String) -> Self {
        Self {
            kind,
            text,
            trailing: String::new(),
            key: None,
            group: false,
            file_line: None,
        }
    }

    /// The line as the dump writes it, without the selection mark.
    pub fn plain(&self) -> String {
        match self.kind {
            LineKind::Heading => self.text.clone(),
            LineKind::Problem => format!("  {}", self.text),
            LineKind::Task => format!("{}  [{}]", self.text, self.trailing),
            LineKind::Detail | LineKind::Edge => format!("    {}", self.text),
        }
    }
}

/// One level of the proposal: the crumbs down to it and its lines.
#[derive(Clone, Debug, Default)]
pub(crate) struct Level {
    /// The labels from the root down to the node shown.
    pub crumbs: Vec<String>,
    pub lines: Vec<Line>,
}

impl Level {
    pub fn breadcrumb(&self) -> String {
        self.crumbs.join(" > ")
    }

    /// The keys of the lines that can be selected, in order.
    pub fn rows(&self) -> Vec<Key> {
        self.lines
            .iter()
            .filter_map(|line| line.key.clone())
            .collect()
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn tasks_below(node: &Node) -> usize {
    node.children
        .iter()
        .map(|child| 1 + tasks_below(child))
        .sum()
}

fn todos_in(node: &Node) -> usize {
    node.todos.len() + node.children.iter().map(todos_in).sum::<usize>()
}

/// The problem counts of the file as one phrase, such as `2 errors, 1 warning`.
fn problem_summary(problems: &[Problem]) -> String {
    let errors = problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .count();
    let warnings = problems.len() - errors;
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(plural(errors, "error", "errors"));
    }
    if warnings > 0 {
        parts.push(plural(warnings, "warning", "warnings"));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

fn edge_text(edge: &Edge) -> String {
    let word = match edge.kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    };
    let mut text = format!("{word} {}", edge.target.join("/"));
    if let Some(unblock) = edge.unblock {
        text.push_str(&format!(" (unblock {})", unblock_name(unblock)));
    }
    text
}

fn todo_text(todo: &bongos_builder_model::Todo) -> String {
    let mut text = format!("todo {}", todo.name);
    let mut attributes = Vec::new();
    if let Some(role) = todo.role {
        attributes.push(role_name(role).to_string());
    }
    if let Some(workflow) = &todo.workflow {
        attributes.push(format!("workflow {workflow}"));
    }
    if let Some(mark) = todo.mark {
        attributes.push(
            match mark {
                bongos_builder_model::TodoMark::Done => "done",
                bongos_builder_model::TodoMark::Skipped => "skipped",
                bongos_builder_model::TodoMark::Cancelled => "cancelled",
            }
            .to_string(),
        );
    }
    if todo.started {
        attributes.push("started".to_string());
    }
    if !attributes.is_empty() {
        text.push_str(&format!(" ({})", attributes.join(", ")));
    }
    text
}

/// The node at `path` below the root, as far as the labels name children. Returns the node and
/// the labels that were followed.
fn descend<'a>(root: &'a Node, path: &[String]) -> (&'a Node, Vec<String>) {
    let mut node = root;
    let mut followed = Vec::new();
    for label in path {
        match node.children.iter().find(|child| &child.label == label) {
            Some(child) => {
                node = child;
                followed.push(label.clone());
            }
            None => break,
        }
    }
    (node, followed)
}

/// The lines of the level `path` names: the problems, then one row per task of the node shown
/// (the node itself when it is a leaf) with its edges and, for a leaf, its result and todos.
/// Labels of `path` that name no task are dropped from the crumbs.
pub(crate) fn build(analysis: &Analysis, path: &[String]) -> Level {
    let parsed = &analysis.parsed;
    let mut lines = Vec::new();
    let mut heading = Line::new(
        LineKind::Heading,
        format!("problems: {}", problem_summary(&analysis.problems)),
    );
    heading.trailing = String::new();
    lines.push(heading);
    for (index, problem) in analysis.problems.iter().enumerate() {
        let severity = match problem.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let mut line = Line::new(
            LineKind::Problem,
            format!("line {}: {severity}: {}", problem.line, problem.message),
        );
        line.key = Some(Key::Problem(index));
        line.file_line = Some(problem.line);
        lines.push(line);
    }

    let root = &parsed.root;
    if root.line == 0 {
        return Level {
            crumbs: vec!["proposal".to_string()],
            lines,
        };
    }
    let (node, followed) = descend(root, path);
    let mut crumbs = vec![root.label.clone()];
    crumbs.extend(followed.iter().cloned());
    // The end of the lines the node owns: the line before its next sibling, found by walking
    // down from the root, so a problem counts for the task whose lines it is on.
    let mut end = usize::MAX;
    let mut walker = root;
    for label in &followed {
        let Some(index) = walker
            .children
            .iter()
            .position(|child| &child.label == label)
        else {
            break;
        };
        if let Some(next) = walker.children.get(index + 1) {
            end = next.line.saturating_sub(1);
        }
        walker = &walker.children[index];
    }
    let (shown, ends): (Vec<&Node>, Vec<usize>) = if node.children.is_empty() {
        (vec![node], vec![end])
    } else {
        let ends = node
            .children
            .iter()
            .enumerate()
            .map(|(index, _)| {
                node.children
                    .get(index + 1)
                    .map_or(end, |next| next.line.saturating_sub(1))
            })
            .collect();
        (node.children.iter().collect(), ends)
    };
    for (child, end) in shown.into_iter().zip(ends) {
        push_task(analysis, child, end, &mut lines);
    }
    Level { crumbs, lines }
}

fn push_task(analysis: &Analysis, node: &Node, end: usize, lines: &mut Vec<Line>) {
    let fact = node
        .attributes
        .id
        .as_deref()
        .and_then(|id| analysis.facts.get(id));
    let mark = fact.map_or(Mark::New, |fact| fact.mark);
    let kind = node
        .attributes
        .kind
        .or(fact.map(|fact| fact.kind))
        .unwrap_or(Kind::PrShip);
    let group = !node.children.is_empty();
    let problems = analysis
        .problems
        .iter()
        .filter(|problem| problem.line >= node.line && problem.line <= end)
        .count();
    let mut trailing = vec![mark.word().to_string(), kind_name(kind).to_string()];
    if group {
        trailing.push(plural(tasks_below(node), "task", "tasks"));
        trailing.push(plural(todos_in(node), "todo", "todos"));
    }
    if problems > 0 {
        trailing.push(plural(problems, "problem", "problems"));
    }
    let mut row = Line::new(LineKind::Task, format!("{}: {}", node.label, node.goal));
    row.trailing = trailing.join(", ");
    row.key = Some(Key::Task(node.label.clone()));
    row.group = group;
    lines.push(row);
    for edge in &node.edges {
        lines.push(Line::new(LineKind::Edge, edge_text(edge)));
    }
    if !group {
        for result in &node.results {
            lines.push(Line::new(
                LineKind::Detail,
                format!("result {}", result.value),
            ));
        }
        for todo in &node.todos {
            lines.push(Line::new(LineKind::Detail, todo_text(todo)));
        }
    }
}

/// Where the text of the view came from, for what it can say when there is none.
enum Source {
    Loading,
    Text(String),
    Unreadable(String),
}

/// A workspace item showing one proposal file one level at a time.
pub struct ProposalView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    task_id: String,
    number: u32,
    file: PathBuf,
    source: Source,
    analysis: Option<Analysis>,
    /// The labels descended into below the root.
    path: Vec<String>,
    level: Level,
    selected: Option<Key>,
    /// The open buffer of the file the text is read from, when there is one.
    buffer: Option<WeakEntity<Buffer>>,
    buffer_subscription: Option<Subscription>,
    /// Counts the disk reads started, so a late answer to an older one is dropped.
    reads: usize,
    _read: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<()> for ProposalView {}

impl ProposalView {
    fn new(workspace: &Workspace, chosen: Chosen, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let mut subscriptions = Vec::new();
        if let Some(store) = WorkStore::global(cx) {
            subscriptions.push(cx.subscribe(&store, |this, _, _, cx| this.refresh(cx)));
        }
        let buffers = project.read(cx).buffer_store().clone();
        subscriptions.push(cx.subscribe(&buffers, |this, _, event, cx| {
            if matches!(
                event,
                BufferStoreEvent::BufferAdded(_) | BufferStoreEvent::BufferDropped(_)
            ) {
                this.refresh(cx);
            }
        }));
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            fs: workspace.app_state().fs.clone(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            task_id: chosen.task_id,
            number: chosen.number,
            file: chosen.path,
            source: Source::Loading,
            analysis: None,
            path: Vec::new(),
            level: Level::default(),
            selected: None,
            buffer: None,
            buffer_subscription: None,
            reads: 0,
            _read: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// The number of the proposal shown.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The file shown.
    pub fn file(&self) -> &PathBuf {
        &self.file
    }

    /// The id of the task whose directory holds the file.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The open buffer of the file, if the project has one.
    fn open_buffer(&self, cx: &App) -> Option<Entity<Buffer>> {
        let project = self.project.read(cx);
        let (worktree, path) = project.find_worktree(&self.file, cx)?;
        let project_path = ProjectPath {
            worktree_id: worktree.read(cx).id(),
            path,
        };
        project.get_open_buffer(&project_path, cx)
    }

    /// Finds where the text comes from now (the open buffer, else the disk) and shows it.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.open_buffer(cx) {
            Some(buffer) => {
                let known = self
                    .buffer
                    .as_ref()
                    .is_some_and(|known| known.entity_id() == buffer.entity_id());
                if !known {
                    self.buffer = Some(buffer.downgrade());
                    self.buffer_subscription = Some(cx.subscribe(
                        &buffer,
                        |this, buffer, event: &BufferEvent, cx| {
                            if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                                this.set_text(buffer.read(cx).text(), cx);
                            }
                        },
                    ));
                }
                // Whatever read was under way is stale: the buffer is the truth now.
                self.reads += 1;
                self._read = Task::ready(());
                self.set_text(buffer.read(cx).text(), cx);
            }
            None => {
                self.buffer = None;
                self.buffer_subscription = None;
                self.read_disk(cx);
            }
        }
    }

    fn read_disk(&mut self, cx: &mut Context<Self>) {
        self.reads += 1;
        let read = self.reads;
        let fs = self.fs.clone();
        let file = self.file.clone();
        self._read = cx.spawn(async move |this, cx| {
            let text = fs.load(&file).await;
            this.update(cx, |this, cx| {
                if this.reads != read || this.buffer.is_some() {
                    return;
                }
                match text {
                    Ok(text) => this.set_text(text, cx),
                    Err(error) => {
                        this.source = Source::Unreadable(format!(
                            "Could not read {}: {error}",
                            this.file.display()
                        ));
                        this.analysis = None;
                        this.rebuild(cx);
                    }
                }
            })
            .ok();
        });
    }

    fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.source = Source::Text(text);
        self.rebuild(cx);
    }

    /// Analyzes the text against the tree and builds the level again, keeping the selection when
    /// its line is still there.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let tree = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
        self.analysis = match (&self.source, &tree) {
            (Source::Text(text), Some(tree)) => Some(analyze(
                text,
                self.number,
                &self.task_id,
                tree,
                TaskBuilderSettings::get_global(cx).todo_warning,
            )),
            _ => None,
        };
        match &self.analysis {
            Some(analysis) => {
                self.level = build(analysis, &self.path);
                // Labels that are gone from the file end the path there.
                self.path = self.level.crumbs.iter().skip(1).cloned().collect();
            }
            None => self.level = Level::default(),
        }
        let rows = self.level.rows();
        if !self
            .selected
            .as_ref()
            .is_some_and(|selected| rows.contains(selected))
        {
            self.selected = self.first_row();
        }
        cx.emit(());
        cx.notify();
    }

    /// The first task row, else the first problem.
    fn first_row(&self) -> Option<Key> {
        let rows = self.level.rows();
        rows.iter()
            .find(|key| matches!(key, Key::Task(_)))
            .or(rows.first())
            .cloned()
    }

    fn select(&mut self, key: Key, cx: &mut Context<Self>) {
        let index = self
            .level
            .lines
            .iter()
            .position(|line| line.key.as_ref() == Some(&key));
        self.selected = Some(key);
        if let Some(index) = index {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let rows = self.level.rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|key| key == selected));
        let next = match current {
            Some(current) => (current as isize + offset).clamp(0, rows.len() as isize - 1) as usize,
            None if offset < 0 => rows.len() - 1,
            None => 0,
        };
        self.select(rows[next].clone(), cx);
    }

    /// RET on the selected line: a problem opens the file at its line, a group row descends.
    pub(crate) fn descend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        self.activate(key, window, cx);
    }

    fn activate(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            Key::Problem(_) => {
                let line = self
                    .level
                    .lines
                    .iter()
                    .find(|line| line.key.as_ref() == Some(&key))
                    .and_then(|line| line.file_line);
                if let Some(line) = line {
                    self.open_file_at(line, window, cx);
                }
            }
            Key::Task(label) => {
                let group = self
                    .level
                    .lines
                    .iter()
                    .any(|line| line.key == Some(Key::Task(label.clone())) && line.group);
                if group {
                    self.path.push(label);
                    self.selected = None;
                    self.rebuild(cx);
                    self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
                }
            }
        }
    }

    /// `^`: back to the parent level, with the task that was left selected.
    pub(crate) fn up(&mut self, cx: &mut Context<Self>) {
        let Some(left) = self.path.pop() else {
            return;
        };
        self.selected = Some(Key::Task(left));
        self.rebuild(cx);
    }

    /// The labels descended into below the root.
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// The breadcrumb, such as `billing > engine`.
    pub fn breadcrumb(&self) -> String {
        self.level.breadcrumb()
    }

    /// The text of every line of the level, without the breadcrumb.
    pub fn lines(&self) -> Vec<String> {
        self.level.lines.iter().map(Line::plain).collect()
    }

    /// The label of the selected task row.
    pub fn selected_task(&self) -> Option<&str> {
        match &self.selected {
            Some(Key::Task(label)) => Some(label),
            _ => None,
        }
    }

    /// The view as text: the proposal, the breadcrumb, then the lines, with `>` before the
    /// selected one.
    pub fn view_text(&self) -> String {
        let mut text = format!("proposal {}\n", self.number);
        match &self.source {
            Source::Loading => text.push_str("loading\n"),
            Source::Unreadable(message) => {
                text.push_str(message);
                text.push('\n');
            }
            Source::Text(_) if self.analysis.is_none() => {
                text.push_str("The Work tree is not loaded\n");
            }
            Source::Text(_) => {
                text.push_str(&format!("breadcrumb: {}\n", self.level.breadcrumb()));
                for line in &self.level.lines {
                    text.push(if line.key.is_some() && line.key == self.selected {
                        '>'
                    } else {
                        ' '
                    });
                    text.push_str(&line.plain());
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Opens the raw file at the 1-based `line`.
    fn open_file_at(&self, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        open_file_at(self.workspace.clone(), self.file.clone(), line, window, cx);
    }

    fn render_line(&self, index: usize, line: &Line, cx: &mut Context<Self>) -> AnyElement {
        let selected = line.key.is_some() && line.key == self.selected;
        let id = ElementId::NamedInteger("proposal-line".into(), index as u64);
        let content = match line.kind {
            LineKind::Heading => {
                let color = if self.level.lines.len() > 1 {
                    Color::Error
                } else {
                    Color::Muted
                };
                div()
                    .w_full()
                    .child(
                        Label::new(line.text.clone())
                            .size(LabelSize::Small)
                            .color(color),
                    )
                    .into_any_element()
            }
            LineKind::Problem => div()
                .pl_3()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .into_any_element(),
            LineKind::Task => h_flex()
                .w_full()
                .gap_2()
                .justify_between()
                .child(Label::new(line.text.clone()).truncate())
                .child(
                    Label::new(line.trailing.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            LineKind::Edge => div()
                .pl_3()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .italic()
                        .truncate(),
                )
                .into_any_element(),
            LineKind::Detail => div()
                .pl_6()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .into_any_element(),
        };
        let item = ListItem::new(id)
            .inset(true)
            .spacing(ListItemSpacing::Dense)
            .toggle_state(selected)
            .child(content);
        match line.key.clone() {
            Some(key) => item
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        this.select(key.clone(), cx);
                        if event.click_count() >= 2 {
                            this.activate(key.clone(), window, cx);
                        }
                    }),
                )
                .into_any_element(),
            None => item.into_any_element(),
        }
    }

    fn render_breadcrumb(&self) -> impl IntoElement {
        h_flex().w_full().gap_1().px_2().py_1().child(
            Label::new(self.level.breadcrumb())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }

    fn message(&self, text: String) -> AnyElement {
        div()
            .p_3()
            .child(Label::new(text).color(Color::Muted))
            .into_any_element()
    }
}

/// Opens `file` in an editor of `workspace` with the cursor at the 1-based `line`.
pub(crate) fn open_file_at(
    workspace: WeakEntity<Workspace>,
    file: PathBuf,
    line: usize,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let open = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    file,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..OpenOptions::default()
                    },
                    window,
                    cx,
                )
            });
            let result = async {
                let item = open?.await?;
                if let Some(editor) = item.downcast::<Editor>() {
                    editor.update_in(cx, |editor, window, cx| {
                        editor.go_to_singleton_buffer_point(
                            Point::new(line.saturating_sub(1) as u32, 0),
                            window,
                            cx,
                        )
                    })?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(
                            workspace,
                            format!("Could not open the proposal: {error}"),
                            cx,
                        )
                    })
                    .ok();
            }
        })
        .detach();
}

impl Render for ProposalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.source {
            Source::Loading => self.message("Reading the proposal".to_string()),
            Source::Unreadable(message) => self.message(message.clone()),
            Source::Text(_) if self.analysis.is_none() => {
                self.message("The Work tree is not loaded".to_string())
            }
            Source::Text(_) => uniform_list(
                "proposal-lines",
                self.level.lines.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    let lines: Vec<(usize, Line)> = range
                        .filter_map(|index| Some((index, this.level.lines.get(index)?.clone())))
                        .collect();
                    lines
                        .into_iter()
                        .map(|(index, line)| this.render_line(index, &line, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element(),
        };
        v_flex()
            .id("proposal-view")
            .key_context("ProposalView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.select_offset(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.select_offset(-1, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.descend(window, cx)))
            .on_action(
                cx.listener(|this, _: &crate::proposal::Descend, window, cx| {
                    this.descend(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::proposal::Up, _, cx| this.up(cx)))
            .size_full()
            .child(self.render_breadcrumb())
            .child(body)
    }
}

impl Focusable for ProposalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ProposalView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match self.analysis.as_ref().map(|analysis| &analysis.parsed.root) {
            Some(root) if !root.label.is_empty() => {
                format!("Proposal {}: {}", self.number, root.label).into()
            }
            _ => format!("Proposal {}", self.number).into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// Shows proposal `chosen` in the workspace: the open view of the same file when there is one,
/// else a new item in the active pane.
pub(crate) fn show(
    chosen: Chosen,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<ProposalView>(cx)
        .find(|view| view.read(cx).file == chosen.path);
    if let Some(view) = existing {
        view.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| ProposalView::new(workspace, chosen, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// `task: show proposal`.
pub(crate) fn show_proposal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    choose::with_proposal(workspace, window, cx, Box::new(show));
}

/// `proposal: descend`, for when the view is not focused: the active item descends.
pub(crate) fn descend_active(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(view) = workspace.active_item_as::<ProposalView>(cx) {
        view.update(cx, |view, cx| view.descend(window, cx));
    }
}

/// `proposal: up`, for when the view is not focused: the active item goes up.
pub(crate) fn up_active(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(view) = workspace.active_item_as::<ProposalView>(cx) {
        view.update(cx, |view, cx| view.up(cx));
    }
}
