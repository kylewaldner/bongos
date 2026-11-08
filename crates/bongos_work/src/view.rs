//! What the Work panel shows (D23): one level of the task tree as a flat list of one-line items.
//! Pure functions over the snapshot and the numbers the registries report; nothing here reads a
//! file or touches the UI, so the text of every row is the same in the panel, in
//! `work: dump view` and in the tests.
//!
//! A row is a title line (goal and state, or goal and age) and a detail line (the rest), so every
//! item of the list is the same height. A row the owner expanded gets more lines after them.

use std::collections::{BTreeMap, HashMap, HashSet};

use bongos_work_model::activity::{
    Entry, Freshness, Item, created_at, is_closed, top_level, tree_activity_time,
};
use bongos_work_model::artifacts::{GUARANTEE, task_artifacts};
use bongos_work_model::commands::kind_name;
use bongos_work_model::derive::{self, Shape, State};
use bongos_work_model::graph::{child_order, level_graph, state_label};
use bongos_work_model::rollup::{Rollup, rollup};
use bongos_work_model::store::{Activity, Task, WorkTree};
use bongos_work_model::{ResultKind, ResultState, TaskResult, TodoState, Visibility};
use chrono::{DateTime, TimeDelta, Utc};

use crate::filters::Filters;
use crate::rebase::RebaseState;
use crate::registry::LiveModes;

/// The parts of the top level that start folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// Open items idle for the stale threshold.
    Stale,
    /// Items that are done, cancelled or archived.
    Closed,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::Stale => "Stale",
            Section::Closed => "Closed",
        }
    }
}

/// What a row stands for: a task, the heading of an activity's one-offs at the top level, or the
/// heading of a folded section of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A task, by id.
    Task(String),
    /// An activity, by slug.
    Activity(String),
    /// A section of the top level.
    Section(Section),
}

/// The text shown above the guarantee of every task with artifacts while a session is open.
pub(crate) const LIVE_SESSION_NOTE: &str = "Changes apply to new sessions and the next resume.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// An `after` or `stacked on` marker between two sibling rows.
    Edge,
    /// The goal of a row, with its state or age.
    Title,
    /// The rest of a row.
    Detail,
    /// A line of the graph that is not a row: the top and bottom of a box, an arrow.
    Graph,
    /// A line of an expanded row.
    Extra,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The row the line belongs to. `None` for an edge marker, and for a task whose `task.toml`
    /// has never parsed (nothing to drill into or focus).
    pub key: Option<Key>,
    pub kind: LineKind,
    pub text: String,
    /// Shown at the right of a title line.
    pub trailing: String,
    /// What a tooltip says when the line is cut off.
    pub full: Option<String>,
    /// Whether the row has gone quiet (idle for the fade threshold) and is shown dimmed.
    pub faded: bool,
}

impl Line {
    fn new(key: Option<Key>, kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            key,
            kind,
            text: text.into(),
            trailing: String::new(),
            full: None,
            faded: false,
        }
    }

    /// The line as plain text.
    pub fn plain(&self) -> String {
        match self.kind {
            LineKind::Edge => format!("  {}", self.text),
            LineKind::Title if self.trailing.is_empty() => self.text.clone(),
            LineKind::Title => format!("{}  {}", self.text, self.trailing),
            LineKind::Detail => format!("    {}", self.text),
            LineKind::Graph => self.text.clone(),
            LineKind::Extra => format!("      {}", self.text),
        }
    }
}

/// One level, ready to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Level {
    /// The drill path that was resolved: the requested one up to the first key that no longer
    /// names a group.
    pub path: Vec<Key>,
    /// The breadcrumb: `Work`, then the name of each key of `path`.
    pub crumbs: Vec<String>,
    pub lines: Vec<Line>,
    /// Whether the row is a group (RET drills in) rather than a leaf (RET focuses it), by key.
    pub groups: HashMap<Key, bool>,
    /// Whether the lines draw a graph (they are a list otherwise).
    pub graph: bool,
}

impl Level {
    /// The keys of the rows, in order.
    pub fn rows(&self) -> Vec<Key> {
        let mut rows = Vec::new();
        for line in &self.lines {
            if line.kind == LineKind::Title
                && let Some(key) = &line.key
            {
                rows.push(key.clone());
            }
        }
        rows
    }

    pub fn is_group(&self, key: &Key) -> bool {
        self.groups.get(key).copied().unwrap_or(false)
    }

    /// The breadcrumb as text, such as `Work > Invoicing > Phase`.
    pub fn breadcrumb(&self) -> String {
        self.crumbs.join(" > ")
    }
}

/// What the registries and the store know about tasks, asked for by id.
pub struct Inputs<'a> {
    pub tree: &'a WorkTree,
    pub needs_you: &'a BTreeMap<String, Vec<String>>,
    pub rebase: &'a HashMap<String, RebaseState>,
    pub live_modes: &'a dyn Fn(&str) -> LiveModes,
    pub changed_files: &'a dyn Fn(&str) -> Option<usize>,
    pub now: DateTime<Utc>,
    pub expanded: &'a HashSet<Key>,
    pub filters: &'a Filters,
    /// Idle for this long fades an open item.
    pub fade: TimeDelta,
    /// Idle for this long moves an open item of the top level to Stale.
    pub stale: TimeDelta,
    /// Whether a group is drawn as a graph rather than listed.
    pub graph: bool,
}

enum Node<'a> {
    Top,
    Activity(&'a Activity),
    Group(&'a Task),
}

/// Follows `path` down from the top level, stopping at the first key that names nothing or a
/// task that is no longer a group.
fn resolve<'a>(tree: &'a WorkTree, path: &[Key]) -> (Node<'a>, usize) {
    let mut node = Node::Top;
    let mut valid = 0;
    for key in path {
        let next = match (&node, key) {
            (Node::Top, Key::Activity(slug)) => tree
                .activities
                .iter()
                .find(|activity| activity.slug == *slug)
                .map(Node::Activity),
            (Node::Top, Key::Task(id)) => tree
                .projects
                .iter()
                .map(|project| &project.task)
                .chain(
                    tree.activities
                        .iter()
                        .flat_map(|activity| activity.oneoffs.iter()),
                )
                .find(|task| task.id() == Some(id))
                .filter(|task| derive::shape(task, tree) == Shape::Group)
                .map(Node::Group),
            (Node::Activity(activity), Key::Task(id)) => activity
                .oneoffs
                .iter()
                .find(|task| task.id() == Some(id))
                .filter(|task| derive::shape(task, tree) == Shape::Group)
                .map(Node::Group),
            (Node::Group(group), Key::Task(id)) => group
                .children
                .iter()
                .find(|task| task.id() == Some(id))
                .filter(|task| derive::shape(task, tree) == Shape::Group)
                .map(Node::Group),
            _ => None,
        };
        match next {
            Some(next) => {
                node = next;
                valid += 1;
            }
            None => break,
        }
    }
    (node, valid)
}

/// The level `path` names, or the deepest level of it that still exists.
pub fn build(path: &[Key], inputs: &Inputs) -> Level {
    let tree = inputs.tree;
    let (node, valid) = resolve(tree, path);
    let path = path[..valid].to_vec();
    let mut crumbs = vec!["Work".to_string()];
    for index in 0..path.len() {
        crumbs.push(match resolve(tree, &path[..=index]).0 {
            Node::Activity(activity) => activity.slug.clone(),
            Node::Group(group) => group_name(group),
            Node::Top => String::new(),
        });
    }

    let needs_set: HashSet<String> = inputs.needs_you.keys().cloned().collect();
    let mut level = Level {
        path,
        crumbs,
        ..Level::default()
    };
    match node {
        Node::Top => add_top(&mut level, inputs, &needs_set),
        Node::Activity(activity) => {
            let mut oneoffs: Vec<&Task> = activity
                .oneoffs
                .iter()
                .filter(|task| inputs.filters.passes(task, tree))
                .collect();
            oneoffs.sort_by_key(|task| (created_at(task), task.slug.clone()));
            for task in oneoffs {
                add_member(&mut level, task, None, inputs, &needs_set);
            }
        }
        Node::Group(group) if inputs.graph => {
            level.graph = true;
            add_graph(&mut level, group, inputs);
        }
        Node::Group(group) => {
            for task in child_order(group) {
                if inputs.filters.passes(task, tree) {
                    add_member(&mut level, task, Some(group), inputs, &needs_set);
                }
            }
        }
    }
    level
}

/// Adds the row of a task inside a group or an activity, dimmed when it has gone quiet. Only the
/// top level moves a quiet task away; here it keeps its place.
fn add_member(
    level: &mut Level,
    task: &Task,
    parent: Option<&Task>,
    inputs: &Inputs,
    needs_set: &HashSet<String>,
) {
    let start = level.lines.len();
    add_task(level, task, parent, false, inputs, needs_set);
    let quiet = !is_closed(task, inputs.tree)
        && tree_activity_time(task, inputs.tree)
            .is_some_and(|time| inputs.now - time >= inputs.fade);
    if quiet {
        fade_from(level, start);
    }
}

fn fade_from(level: &mut Level, start: usize) {
    for line in &mut level.lines[start..] {
        line.faded = true;
    }
}

/// The top level: open items by freshness, then the Stale and Closed sections, each folded unless
/// the filters say to show it.
fn add_top<'a>(level: &mut Level, inputs: &Inputs<'a>, needs_set: &HashSet<String>) {
    let top = top_level(inputs.tree, inputs.now, inputs.fade, inputs.stale);
    let passing = |entries: &[Entry<'a>]| -> Vec<Entry<'a>> {
        entries
            .iter()
            .filter(|entry| item_passes(&entry.item, inputs))
            .copied()
            .collect()
    };
    for entry in passing(&top.open) {
        add_entry(level, &entry, inputs, needs_set);
    }
    for (section, entries, open) in [
        (
            Section::Stale,
            passing(&top.stale),
            inputs.filters.show_stale,
        ),
        (
            Section::Closed,
            passing(&top.closed),
            inputs.filters.show_closed,
        ),
    ] {
        if entries.is_empty() {
            continue;
        }
        let mut heading = Line::new(
            Some(Key::Section(section)),
            LineKind::Title,
            format!("{} ({})", section.name(), entries.len()),
        );
        if !open {
            heading.trailing = "folded".to_string();
        }
        level.lines.push(heading);
        if open {
            for entry in &entries {
                add_entry(level, entry, inputs, needs_set);
            }
        }
    }
}

fn item_passes(item: &Item, inputs: &Inputs) -> bool {
    let filters = inputs.filters;
    match item {
        Item::Project(project) => filters.passes(&project.task, inputs.tree),
        Item::Activity(activity) => activity
            .oneoffs
            .iter()
            .any(|oneoff| filters.passes(oneoff, inputs.tree)),
        Item::Task(task) => filters.passes(task, inputs.tree),
    }
}

/// Adds the row of a top-level item, dimmed while it is fading.
fn add_entry(level: &mut Level, entry: &Entry, inputs: &Inputs, needs_set: &HashSet<String>) {
    let start = level.lines.len();
    match entry.item {
        Item::Project(project) => {
            let archived = project.file.as_ref().is_some_and(|file| file.archived);
            add_task(level, &project.task, None, archived, inputs, needs_set);
        }
        Item::Activity(activity) => add_activity(level, activity, inputs, needs_set),
        Item::Task(task) => add_task(level, task, None, false, inputs, needs_set),
    }
    if entry.freshness == Freshness::Fading {
        fade_from(level, start);
    }
}

/// The boxes and arrows of `group`. The middle line of a box is its row; every line of a box
/// carries its key, so a click anywhere on it selects it.
fn add_graph(level: &mut Level, group: &Task, inputs: &Inputs) {
    for line in level_graph(group, inputs.tree) {
        let key = line.node.clone().map(Key::Task);
        let label = line.text.starts_with("| ");
        let kind = if label {
            LineKind::Title
        } else {
            LineKind::Graph
        };
        if let (Some(id), true) = (&line.node, label)
            && let Some(task) = group.children.iter().find(|task| task.id() == Some(id))
        {
            level.groups.insert(
                Key::Task(id.clone()),
                derive::shape(task, inputs.tree) == Shape::Group,
            );
        }
        level.lines.push(Line::new(key, kind, line.text));
    }
}

fn group_name(group: &Task) -> String {
    match group.goal() {
        "" => group.slug.clone(),
        goal => goal.to_string(),
    }
}

/// The age as a row shows it: `now`, `5m`, `3h` or `2d`.
pub(crate) fn age_text(now: DateTime<Utc>, time: DateTime<Utc>) -> String {
    let minutes = (now - time).num_minutes();
    if minutes < 1 {
        "now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (24 * 60))
    }
}

/// The roll-up as a row shows it, such as `5/9, 1 blocked, 1 needs you`.
pub(crate) fn rollup_text(rollup: &Rollup) -> String {
    let mut text = rollup.progress_text();
    if rollup.blocked > 0 {
        text.push_str(&format!(", {} blocked", rollup.blocked));
    }
    if rollup.needs_you > 0 {
        text.push_str(&format!(", {} needs you", rollup.needs_you));
    }
    text
}

fn result_text(result: &TaskResult) -> String {
    let kind = match result.kind {
        ResultKind::Pr => "PR",
        ResultKind::Prs => "PRs",
        ResultKind::Document => "document",
        ResultKind::Link => "link",
        ResultKind::Findings => "findings",
        ResultKind::Branch => "branch",
    };
    match result.state {
        Some(state) => {
            let state = match state {
                ResultState::Open => "open",
                ResultState::Approved => "approved",
                ResultState::Merged => "merged",
                ResultState::Closed => "closed",
            };
            format!("{kind} {state}")
        }
        None => kind.to_string(),
    }
}

/// The edge markers before the row of `task` among the children of `parent`.
fn edge_lines(task: &Task, parent: &Task) -> Vec<Line> {
    let Some(file) = &task.file else {
        return Vec::new();
    };
    let target = |slug: &str| {
        parent
            .children
            .iter()
            .find(|sibling| sibling.slug == slug)
            .map_or_else(|| format!("{slug} (missing)"), group_name)
    };
    let mut lines: Vec<Line> = file
        .after
        .iter()
        .map(|edge| {
            Line::new(
                None,
                LineKind::Edge,
                format!("after {}", target(&edge.task)),
            )
        })
        .collect();
    if let Some(edge) = &file.stacked_on {
        lines.push(Line::new(
            None,
            LineKind::Edge,
            format!("stacked on {}", target(&edge.task)),
        ));
    }
    lines
}

fn add_task(
    level: &mut Level,
    task: &Task,
    parent: Option<&Task>,
    archived: bool,
    inputs: &Inputs,
    needs_set: &HashSet<String>,
) {
    let tree = inputs.tree;
    if let Some(parent) = parent {
        level.lines.extend(edge_lines(task, parent));
    }
    let key = task.id().map(|id| Key::Task(id.to_string()));
    let shape = derive::shape(task, tree);
    let mut title = Line::new(key.clone(), LineKind::Title, group_name(task));
    let mut details: Vec<String> = Vec::new();
    match shape {
        Shape::Group => {
            let roll = rollup(task, tree, needs_set);
            details.push(rollup_text(&roll));
            if archived {
                details.push("archived".to_string());
            }
            if let Some(time) = tree_activity_time(task, tree) {
                title.trailing = age_text(inputs.now, time);
            }
        }
        Shape::Leaf => {
            title.trailing = state_label(derive::state(task, tree)).to_string();
            details = leaf_details(task, inputs);
        }
    }
    if !task.errors.is_empty() {
        details.push(format!("error: {}", task.errors[0].reason));
    }
    if let Some(key) = &key {
        level.groups.insert(key.clone(), shape == Shape::Group);
    }
    level.lines.push(title);
    level.lines.push(Line::new(
        key.clone(),
        LineKind::Detail,
        details.join(" · "),
    ));
    if let Some(key) = &key
        && inputs.expanded.contains(key)
    {
        add_expansion(level, key, task, inputs);
    }
}

fn leaf_details(task: &Task, inputs: &Inputs) -> Vec<String> {
    let tree = inputs.tree;
    let mut details = Vec::new();
    let Some(file) = &task.file else {
        return details;
    };
    details.push(kind_name(file.kind).to_string());
    let todo = derive::current_todo(task);
    if let Some(todo) = todo {
        details.push(format!("[{}]", todo.name));
    }
    if let Some(result) = &file.result {
        details.push(result_text(result));
    }
    let id = file.id.as_str();
    if derive::state(task, tree) == State::Blocked {
        let holding: Vec<String> = derive::blocking_targets(task, tree)
            .into_iter()
            .map(group_name)
            .collect();
        details.push(if holding.is_empty() {
            "blocked".to_string()
        } else {
            format!("blocked by {}", holding.join(", "))
        });
    }
    match inputs.rebase.get(id) {
        Some(RebaseState::RebaseNeeded { .. }) => details.push("rebase needed".to_string()),
        Some(RebaseState::RetargetNeeded) => details.push("retarget needed".to_string()),
        None => {}
    }
    if let Some(reasons) = inputs.needs_you.get(id) {
        details.push(match reasons.first() {
            Some(reason) => format!("needs you: {reason}"),
            None => "needs you".to_string(),
        });
    }
    if let Some(branch) = &file.branch {
        details.push(branch.clone());
    }
    let live = (inputs.live_modes)(id);
    match live.open_chats {
        0 => {}
        1 => details.push("1 chat".to_string()),
        chats => details.push(format!("{chats} chats")),
    }
    if let Some(run) = &live.run {
        details.push(run.chip());
    }
    if let Some(changed) = (inputs.changed_files)(id).filter(|count| *count > 0) {
        details.push(format!("{changed} changed"));
    }
    if let Some(workflow) = todo.and_then(|todo| todo.workflow.as_ref()) {
        details.push(format!("workflow {workflow}"));
    }
    if let Some(linear) = &file.linear {
        details.push(linear.clone());
    }
    let (total, locked) = artifact_counts(task);
    if total > 0 {
        details.push(if locked > 0 {
            format!("{total} artifacts, {locked} locked")
        } else {
            format!("{total} artifacts")
        });
    }
    details
}

/// How many artifacts the task has and how many of them are human-only.
fn artifact_counts(task: &Task) -> (usize, usize) {
    let list = task_artifacts(task).artifacts;
    let locked = list
        .iter()
        .filter(|artifact| artifact.visibility == Some(Visibility::HumanOnly))
        .count();
    (list.len(), locked)
}

/// The lines under an expanded row: its todos when it has more than one, then its artifacts and
/// what the owner is promised about them.
fn add_expansion(level: &mut Level, key: &Key, task: &Task, inputs: &Inputs) {
    let extra = |text: String| Line::new(Some(key.clone()), LineKind::Extra, text);
    if let Some(file) = &task.file
        && file.todos.len() > 1
    {
        level.lines.push(extra("Todos".to_string()));
        for todo in &file.todos {
            let mark = match todo.state {
                TodoState::Open => "[ ]",
                TodoState::Done => "[x]",
                TodoState::Skipped => "[-]",
                TodoState::Cancelled => "[~]",
            };
            let mut text = format!("{mark} {}", todo.name);
            if let Some(workflow) = &todo.workflow {
                text.push_str(&format!("  workflow {workflow}"));
            }
            level.lines.push(extra(text));
        }
    }
    let artifacts = task_artifacts(task).artifacts;
    if artifacts.is_empty() {
        return;
    }
    level
        .lines
        .push(extra(format!("Artifacts ({})", artifacts.len())));
    let mut guarantee = extra(GUARANTEE.to_string());
    guarantee.full = Some(GUARANTEE.to_string());
    level.lines.push(guarantee);
    let live = task
        .id()
        .map(|id| (inputs.live_modes)(id))
        .unwrap_or_default();
    if live.open_chats > 0 || live.run.is_some() {
        level.lines.push(extra(LIVE_SESSION_NOTE.to_string()));
    }
    for artifact in artifacts {
        let state = match artifact.visibility {
            Some(Visibility::HumanOnly) => "locked",
            _ => "shared",
        };
        let mut text = format!("{}  {state}", artifact.path);
        if !artifact.description.is_empty() {
            text.push_str(&format!("  {}", artifact.description));
        }
        level.lines.push(extra(text));
    }
}

fn add_activity(
    level: &mut Level,
    activity: &Activity,
    inputs: &Inputs,
    needs_set: &HashSet<String>,
) {
    let tree = inputs.tree;
    let mut total = Rollup::default();
    for oneoff in &activity.oneoffs {
        let one = rollup(oneoff, tree, needs_set);
        total.done += one.done;
        total.total += one.total;
        total.blocked += one.blocked;
        total.needs_you += one.needs_you;
    }
    let key = Key::Activity(activity.slug.clone());
    let mut title = Line::new(Some(key.clone()), LineKind::Title, activity.slug.clone());
    if let Some(time) = activity
        .oneoffs
        .iter()
        .filter_map(|oneoff| tree_activity_time(oneoff, tree))
        .max()
    {
        title.trailing = age_text(inputs.now, time);
    }
    level.groups.insert(key.clone(), true);
    level.lines.push(title);
    level.lines.push(Line::new(
        Some(key),
        LineKind::Detail,
        format!("one-offs, {}", rollup_text(&total)),
    ));
}
