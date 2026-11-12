//! The run view (D20): one run as a workspace item, key context `RunView`. A list of the nodes in
//! run order comes first; `g` switches to the graph, drawn with boxes and edges painted on a
//! canvas. Above them the header (workflow, layer, todo, state, time, cost, caps used); below, the
//! gate cards, the outcomes and a detail pane for the selected node.
//!
//! The view never shows what is in the worktree: no file contents and no diffs. Reading code is
//! `workflow: show run diff` or `workflow: open run worktree`, a deliberate step. What a
//! [`Snapshot`] holds comes from the run's record (state, definition, logs of the nodes), and
//! [`Snapshot::text`] is the debug dump. Nothing in [`Snapshot`] needs a window.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use bongos_work::Focus;
use bongos_workflow_model::definition::{Definition, NodeKind};
use bongos_workflow_model::layout::{GraphLayout, LayoutNode, Status, layout};
use bongos_workflow_model::record::{NodeKey, RunState, branch_label};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Global, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, PathBuilder, Render, SharedString, Styled as _, WeakEntity,
    Window, canvas, div, point, px,
};
use settings::Settings as _;
use ui::{
    Button, ButtonCommon as _, ButtonStyle, Clickable as _, Color, Icon, IconName, Label,
    LabelCommon as _, LabelSize, h_flex, prelude::*, v_flex,
};
use workspace::item::{Item, ItemEvent};
use workspace::{MultiWorkspace, OpenOptions, OpenVisible, Workspace};

use crate::actions::{show_toast, workflow};
use crate::commands::{on, on_run, open_report, open_worktree};
use crate::engine::WorkflowEngine;
use crate::lifecycle::{RunId, RunStatus, definition_copy_dir};
use crate::outcomes::{self, RunOutcome};
use crate::runner_claude::STREAM_FILE;
use crate::runner_program::{STDERR_FILE, STDOUT_FILE};
use crate::worktrees::Git;

/// How many log lines the detail pane shows.
pub const LOG_LINES: usize = 200;
/// How many lines of a prompt or script the detail pane shows.
const SOURCE_LINES: usize = 40;

/// The two ways the nodes are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The nodes in the order they ran.
    #[default]
    List,
    /// The graph of the definition.
    Graph,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Self::List => Self::Graph,
            Self::Graph => Self::List,
        }
    }
}

/// A node box chosen in the view: its node and its fan-out branch, whatever the pass.
pub type Selection = (String, Vec<(String, u32)>);

/// What the view reads of a run, taken from the engine.
#[derive(Debug, Clone)]
pub struct RunData {
    pub run: RunId,
    pub workflow: String,
    pub layer: String,
    pub todo: String,
    pub definition: Definition,
    pub state: RunState,
    pub worktree: Option<PathBuf>,
    pub attempt_dir: Option<PathBuf>,
    /// The sessions the settings allow at once.
    pub session_limit: usize,
}

impl RunData {
    /// The data of `run`, or `None` when the engine does not know it.
    pub fn of(engine: &WorkflowEngine, run: &str, session_limit: usize) -> Option<Self> {
        let lifecycle = engine.lifecycle();
        let state = lifecycle.state(run)?;
        let definition = lifecycle.runtime().definition(run)?.clone();
        let record = lifecycle.record(run)?;
        Some(Self {
            run: run.to_string(),
            workflow: record.workflow.clone(),
            layer: record.layer.clone(),
            todo: record.todo.name.clone(),
            definition,
            state,
            worktree: lifecycle
                .worktree(run)
                .map(|worktree| worktree.path.clone()),
            attempt_dir: lifecycle.current_attempt_dir(run),
            session_limit,
        })
    }
}

/// A node in the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub branch: Vec<(String, u32)>,
    pub status: Status,
    pub passes: u32,
    pub tries: u32,
    pub cost_usd: f64,
}

impl Row {
    pub fn selection(&self) -> Selection {
        (self.name.clone(), self.branch.clone())
    }
}

/// A gate waiting for Kyle.
#[derive(Debug, Clone, PartialEq)]
pub struct GateCard {
    pub key: NodeKey,
    pub question: String,
    /// The typed result the gate holds, when it is an `after` gate.
    pub result: Option<String>,
}

/// What the detail pane shows about one node.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Detail {
    pub name: String,
    pub kind: String,
    pub status: String,
    pub prompt: Option<String>,
    pub script: Option<String>,
    pub result: Option<String>,
    pub permissions: Vec<String>,
    pub caps: Vec<String>,
    pub log: Vec<String>,
}

/// Everything the view shows of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub run: RunId,
    pub workflow: String,
    pub layer: String,
    pub todo: String,
    pub status: RunStatus,
    pub elapsed_ms: u64,
    pub cost_usd: f64,
    pub caps: Vec<String>,
    pub rows: Vec<Row>,
    pub layout: GraphLayout,
    pub gates: Vec<GateCard>,
    pub outcomes: Vec<String>,
    pub failure: Option<String>,
    pub detail: Option<Detail>,
    /// The box the detail pane is about.
    pub selected: Option<Selection>,
}

impl Snapshot {
    /// Builds the snapshot of `data` with `selected` in the detail pane (the first row when it is
    /// `None` or not in the run).
    pub fn build(data: &RunData, selected: Option<&Selection>) -> Self {
        let state = &data.state;
        let graph = layout(&data.definition, state);
        let rows = rows(state, &graph);
        let selected = selected
            .filter(|selected| {
                graph
                    .nodes
                    .iter()
                    .any(|node| (&node.name, &node.branch) == (&selected.0, &selected.1))
            })
            .cloned()
            .or_else(|| rows.first().map(Row::selection));
        let detail = selected
            .as_ref()
            .map(|selected| detail(data, &graph, selected));
        let gates = state
            .open_gates
            .iter()
            .map(|(key, question)| GateCard {
                key: key.clone(),
                question: question.clone(),
                result: state
                    .last_finish(key)
                    .and_then(|finished| finished.result.as_ref())
                    .map(pretty),
            })
            .collect();
        let outcomes = outcomes::collect(&data.definition, state)
            .iter()
            .map(|reported| format!("{}: {}", reported.key.node, outcome_text(&reported.outcome)))
            .collect();
        let failure = match &state.ended {
            Some((_, Some(failure))) => Some(failure.to_string()),
            _ => None,
        };
        Self {
            run: data.run.clone(),
            workflow: data.workflow.clone(),
            layer: data.layer.clone(),
            todo: data.todo.clone(),
            status: RunStatus::of(state),
            elapsed_ms: state
                .finished
                .iter()
                .map(|finished| finished.elapsed_ms)
                .sum(),
            cost_usd: state.cost_usd,
            caps: caps_used(data, &graph),
            rows,
            layout: graph,
            gates,
            outcomes,
            failure,
            detail,
            selected,
        }
    }

    /// The boxes in the order a key press walks them in `mode`.
    pub fn order(&self, mode: Mode) -> Vec<Selection> {
        match mode {
            Mode::List => self.rows.iter().map(Row::selection).collect(),
            Mode::Graph => self
                .layout
                .nodes
                .iter()
                .map(|node| (node.name.clone(), node.branch.clone()))
                .collect(),
        }
    }

    /// The text form of the view: what the dump writes and the tests read.
    pub fn text(&self, mode: Mode) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Run {} ({}) of {}",
            self.workflow, self.layer, self.todo
        );
        let _ = writeln!(
            out,
            "state: {}  time: {}  cost: {}",
            self.status.label(),
            format_ms(self.elapsed_ms),
            format_cost(self.cost_usd)
        );
        if !self.caps.is_empty() {
            let _ = writeln!(out, "caps: {}", self.caps.join("; "));
        }
        if let Some(failure) = &self.failure {
            let _ = writeln!(out, "failure: {failure}");
        }
        match mode {
            Mode::List => {
                let _ = writeln!(out, "nodes (run order)");
                for row in &self.rows {
                    let branch = if row.branch.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", branch_label(&row.branch))
                    };
                    let _ = writeln!(
                        out,
                        "  {}{branch}  {}  pass {}  tries {}  {}",
                        row.name,
                        row.status.label(),
                        row.passes.max(1),
                        row.tries,
                        format_cost(row.cost_usd)
                    );
                }
            }
            Mode::Graph => {
                let _ = writeln!(out, "graph");
                out.push_str(&self.layout.text());
            }
        }
        for gate in &self.gates {
            let _ = writeln!(out, "gate at {}: {}", gate.key.node, gate.question);
            if let Some(result) = &gate.result {
                let _ = writeln!(out, "  result: {result}");
            }
        }
        if !self.outcomes.is_empty() {
            let _ = writeln!(out, "outcomes");
            for outcome in &self.outcomes {
                let _ = writeln!(out, "  {outcome}");
            }
        }
        if let Some(detail) = &self.detail {
            let _ = writeln!(
                out,
                "node {} ({}, {})",
                detail.name, detail.kind, detail.status
            );
            if let Some(prompt) = &detail.prompt {
                let _ = writeln!(out, "  prompt: {prompt}");
            }
            if let Some(script) = &detail.script {
                let _ = writeln!(out, "  script: {script}");
            }
            if let Some(result) = &detail.result {
                let _ = writeln!(out, "  result: {result}");
            }
            if !detail.permissions.is_empty() {
                let _ = writeln!(out, "  permissions: {}", detail.permissions.join("; "));
            }
            if !detail.caps.is_empty() {
                let _ = writeln!(out, "  caps: {}", detail.caps.join("; "));
            }
            if !detail.log.is_empty() {
                let _ = writeln!(out, "  log (last {} lines)", detail.log.len());
                for line in &detail.log {
                    let _ = writeln!(out, "    {line}");
                }
            }
        }
        out
    }
}

fn outcome_text(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Pr { url, .. } => format!("pr {url}"),
        RunOutcome::Document { path } => format!("document {path}"),
        RunOutcome::Tasks { path } => format!("tasks {path}"),
    }
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// The nodes that started, in the order they first appeared in the log.
fn rows(state: &RunState, graph: &GraphLayout) -> Vec<Row> {
    let mut order: Vec<Selection> = Vec::new();
    let keys = state
        .finished
        .iter()
        .map(|finished| &finished.key)
        .chain(state.running.keys())
        .chain(state.open_gates.keys());
    for key in keys {
        let entry = (key.node.clone(), key.branch.clone());
        if !order.contains(&entry) {
            order.push(entry);
        }
    }
    order
        .into_iter()
        .filter_map(|(name, branch)| {
            let node = graph
                .nodes
                .iter()
                .find(|node| node.name == name && node.branch == branch)?;
            let own = |key: &NodeKey| key.node == name && key.branch == branch;
            let finished = state.finished.iter().filter(|finished| own(&finished.key));
            Some(Row {
                tries: finished.clone().count() as u32
                    + state.running.keys().filter(|key| own(key)).count() as u32,
                cost_usd: finished.map(|finished| finished.cost_usd).sum(),
                name,
                branch,
                status: node.status,
                passes: node.passes,
            })
        })
        .collect()
}

fn caps_used(data: &RunData, graph: &GraphLayout) -> Vec<String> {
    let mut caps = vec![format!("cost {}", format_cost(data.state.cost_usd))];
    caps.push(format!(
        "sessions {} of {}",
        data.state.running.len(),
        data.session_limit
    ));
    for arc in &graph.arcs {
        caps.push(format!("{} -> {} {}", arc.from, arc.to, arc.label));
    }
    caps
}

fn detail(data: &RunData, graph: &GraphLayout, selected: &Selection) -> Detail {
    let Some(laid) = graph
        .nodes
        .iter()
        .find(|node| node.name == selected.0 && node.branch == selected.1)
    else {
        return Detail::default();
    };
    let mut detail = Detail {
        name: laid.name.clone(),
        kind: laid.kind.label().to_string(),
        status: laid.status.label().to_string(),
        ..Detail::default()
    };
    let Some(node) = data.definition.node(&laid.name) else {
        return detail;
    };
    let rev = data.state.definition_rev.unwrap_or(0);
    let source_dir = data
        .attempt_dir
        .as_deref()
        .map(|dir| definition_copy_dir(dir, rev));
    let source = |name: &str| {
        let text = source_dir
            .as_deref()
            .and_then(|dir| read_definition_file(dir, name))
            .map(|text| head_lines(&text, SOURCE_LINES));
        match text {
            Some(text) => format!("{name}\n{text}"),
            None => name.to_string(),
        }
    };
    detail.prompt = node.prompt.as_ref().map(|prompt| source(&prompt.value));
    detail.script = node
        .script
        .as_ref()
        .map(|script| source(&script.value))
        .or_else(|| {
            node.use_node
                .as_ref()
                .map(|used| format!("shipped: {}", used.value))
        });
    if let Some(permissions) = &node.permissions {
        for (label, list) in [
            ("commands", &permissions.commands),
            ("network", &permissions.network),
            ("mcp", &permissions.mcp),
        ] {
            if !list.is_empty() {
                detail
                    .permissions
                    .push(format!("{label}: {}", list.join(", ")));
            }
        }
    } else if node.kind.value == NodeKind::Program {
        detail.permissions.push("none declared".to_string());
    }
    let key = laid.key();
    let last = key.as_ref().and_then(|key| {
        data.state
            .finished
            .iter()
            .rev()
            .find(|finished| finished.key.node == key.node && finished.key.branch == key.branch)
    });
    if let Some(last) = last {
        detail.result = last.result.as_ref().map(pretty);
        detail
            .caps
            .push(format!("cost {}", format_cost(last.cost_usd)));
        detail
            .caps
            .push(format!("time {}", format_ms(last.elapsed_ms)));
        if let Some(turns) = last.turns {
            detail.caps.push(format!("turns {turns}"));
        }
        detail.caps.push(format!("try {}", last.try_number));
        if let Some(failure) = &last.failure {
            detail.caps.push(format!("failed: {failure}"));
        }
    }
    // The log is the running try's, else the last finished one's.
    let running = key.as_ref().and_then(|key| {
        data.state
            .running
            .iter()
            .filter(|(running, _)| running.node == key.node && running.branch == key.branch)
            .map(|(running, node)| (running.clone(), node.try_number))
            .next_back()
    });
    let logged = running.or_else(|| last.map(|last| (last.key.clone(), last.try_number)));
    if let (Some(dir), Some((key, try_number))) = (&data.attempt_dir, logged) {
        detail.log = try_log(dir, &key, try_number);
    }
    detail
}

/// A file of the definition's copy, by a name that stays inside the copy.
fn read_definition_file(dir: &Path, name: &str) -> Option<String> {
    let relative = Path::new(name);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    fs::read_to_string(dir.join(relative)).ok()
}

fn head_lines(text: &str, count: usize) -> String {
    text.lines().take(count).collect::<Vec<_>>().join("\n")
}

/// The record directory of a try, `<attempt>/nodes/<node>/<branch>/<pass>-<try>`.
pub fn try_dir(attempt: &Path, key: &NodeKey, try_number: u32) -> PathBuf {
    let base = attempt.join("nodes").join(&key.node);
    let leaf = format!("{}-{try_number}", key.pass);
    let with_branch = base.join(key.branch_label()).join(&leaf);
    if with_branch.exists() || !key.branch.is_empty() {
        with_branch
    } else {
        base.join(leaf)
    }
}

/// The last [`LOG_LINES`] lines of the output of a try (stdout, or the agent's stream, then
/// stderr). These are files of the run's record, never of the worktree.
fn try_log(attempt: &Path, key: &NodeKey, try_number: u32) -> Vec<String> {
    let dir = try_dir(attempt, key, try_number);
    let mut lines: Vec<String> = Vec::new();
    for file in [STDOUT_FILE, STREAM_FILE, STDERR_FILE] {
        if let Ok(text) = fs::read_to_string(dir.join(file)) {
            lines.extend(text.lines().map(str::to_string));
        }
    }
    let skip = lines.len().saturating_sub(LOG_LINES);
    lines.split_off(skip)
}

/// The file `workflow: open logs` opens for a try: its stdout or stream.
pub fn log_file(attempt: &Path, key: &NodeKey, try_number: u32) -> Option<PathBuf> {
    let dir = try_dir(attempt, key, try_number);
    [STDOUT_FILE, STREAM_FILE, STDERR_FILE]
        .iter()
        .map(|file| dir.join(file))
        .find(|path| path.exists())
}

pub(crate) fn format_cost(cost: f64) -> String {
    format!("${cost:.2}")
}

pub(crate) fn format_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, seconds) => format!("{minutes}m {seconds}s"),
        (hours, minutes, _) => format!("{hours}h {minutes}m"),
    }
}

/// The views to refresh when the engine publishes.
#[derive(Default)]
struct RunViews(Vec<WeakEntity<RunView>>);

impl Global for RunViews {}

/// A workspace item showing one run.
pub struct RunView {
    engine: WorkflowEngine,
    run: RunId,
    mode: Mode,
    selected: Option<Selection>,
    focus_handle: FocusHandle,
    snapshot: Option<Snapshot>,
}

impl EventEmitter<()> for RunView {}

impl RunView {
    pub fn new(engine: WorkflowEngine, run: RunId, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            engine,
            run,
            mode: Mode::List,
            selected: None,
            focus_handle: cx.focus_handle(),
            snapshot: None,
        };
        view.refresh(cx);
        view
    }

    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// The box the detail pane is about.
    pub fn selected(&self) -> Option<&Selection> {
        self.selected.as_ref()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// The text the view shows now.
    pub fn text(&self) -> String {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.text(self.mode))
            .unwrap_or_else(|| "The run is no longer known".to_string())
    }

    /// Reads the run again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let limit = crate::WorkflowsSettings::get_global(cx)
            .limits
            .session_limit();
        self.snapshot = RunData::of(&self.engine, &self.run, limit)
            .map(|data| Snapshot::build(&data, self.selected.as_ref()));
        if let Some(snapshot) = &self.snapshot {
            self.selected = snapshot.selected.clone();
        }
        cx.notify();
    }

    /// `g`: the list and the graph swap places.
    pub fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        self.mode = self.mode.toggled();
        cx.notify();
    }

    /// Moves the selection by `offset` boxes in the current mode.
    pub fn select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let order = snapshot.order(self.mode);
        if order.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| order.iter().position(|entry| entry == selected))
            .unwrap_or(0) as isize;
        let next = (current + offset).clamp(0, order.len() as isize - 1) as usize;
        self.selected = Some(order[next].clone());
        self.refresh(cx);
    }

    /// Selects a box.
    pub fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selected = Some(selection);
        self.refresh(cx);
    }

    /// Approves the gate of a card: the same call `workflow: approve` makes.
    pub fn approve_gate(&mut self, key: &NodeKey, cx: &mut Context<Self>) -> Result<(), String> {
        let approved = self.engine.approve(&self.run, key, None, cx);
        self.refresh(cx);
        approved.map_err(|refusal| refusal.to_string())
    }

    fn render_header(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                Label::new(format!(
                    "{} ({}) on {}",
                    snapshot.workflow, snapshot.layer, snapshot.todo
                ))
                .size(LabelSize::Large),
            )
            .child(
                Label::new(format!(
                    "{}  time {}  cost {}",
                    snapshot.status.label(),
                    format_ms(snapshot.elapsed_ms),
                    format_cost(snapshot.cost_usd)
                ))
                .color(status_color(snapshot.status)),
            )
            .child(
                Label::new(format!("caps used: {}", snapshot.caps.join("; ")))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(
                snapshot
                    .failure
                    .as_ref()
                    .map(|failure| Label::new(format!("failed: {failure}")).color(Color::Error)),
            )
    }

    fn render_list(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let selected = self.selected.clone();
        v_flex()
            .p_1()
            .children(snapshot.rows.iter().enumerate().map(|(index, row)| {
                let is_selected = selected.as_ref() == Some(&row.selection());
                let selection = row.selection();
                let branch = if row.branch.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", branch_label(&row.branch))
                };
                h_flex()
                    .id(("run-view-row", index))
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .when(is_selected, |row| row.bg(colors.element_selected))
                    .hover(|row| row.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                    .child(Label::new(format!("{}{branch}", row.name)))
                    .child(Label::new(row.status.label()).color(box_color(row.status)))
                    .child(Label::new(format!("pass {}", row.passes.max(1))).color(Color::Muted))
                    .child(Label::new(format_cost(row.cost_usd)).color(Color::Muted))
            }))
    }

    fn render_graph(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let graph = &snapshot.layout;
        let lanes = graph
            .nodes
            .iter()
            .map(|node| node.lane + 1)
            .max()
            .unwrap_or(1);
        let width = GRAPH_COLUMN * graph.columns.max(1) as f32;
        let height = GRAPH_ROW * lanes as f32 + GRAPH_ARC * (graph.arcs.len() as f32 + 1.0);
        let lines = graph_lines(graph);
        let line_color: Hsla = cx.theme().colors().border;
        let arc_color: Hsla = cx.theme().status().warning;
        let colors = cx.theme().colors();
        let selected = self.selected.clone();
        let boxes = graph.nodes.iter().enumerate().map(|(index, node)| {
            let origin = box_origin(node);
            let is_selected = selected.as_ref() == Some(&(node.name.clone(), node.branch.clone()));
            let selection = (node.name.clone(), node.branch.clone());
            let label = if node.passes > 1 {
                format!("{} (pass {})", node.name, node.passes)
            } else {
                node.name.clone()
            };
            let border = box_border(node.status, cx);
            v_flex()
                .id(("run-view-box", index))
                .absolute()
                .left(px(origin.0))
                .top(px(origin.1))
                .w(px(GRAPH_BOX_WIDTH))
                .h(px(GRAPH_BOX_HEIGHT))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(if is_selected {
                    colors.element_selected
                } else {
                    colors.element_background
                })
                .when(node.status.dimmed(), |element| element.opacity(0.5))
                .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                .child(Label::new(label).size(LabelSize::Small))
                .child(
                    Label::new(format!("{} {}", node.kind.label(), node.status.label()))
                        .size(LabelSize::XSmall)
                        .color(box_color(node.status)),
                )
        });
        let arc_labels = graph.arcs.iter().enumerate().filter_map(|(index, arc)| {
            let (x, _) = arc_span(graph, arc)?;
            Some(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(arc_y(graph, index) + 2.0))
                    .child(
                        Label::new(arc.label.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
            )
        });
        let arcs: Vec<(f32, f32, f32)> = graph
            .arcs
            .iter()
            .enumerate()
            .filter_map(|(index, arc)| {
                let (from, to) = arc_span(graph, arc)?;
                Some((from, to, arc_y(graph, index)))
            })
            .collect();
        let arc_bottoms: Vec<(f32, f32)> = graph
            .arcs
            .iter()
            .filter_map(|arc| arc_ends(graph, arc))
            .collect();
        div()
            .relative()
            .w(px(width))
            .h(px(height))
            .m_2()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let origin = bounds.origin;
                        let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
                        for (from, to) in &lines {
                            let mut path = PathBuilder::stroke(px(1.5));
                            path.move_to(at(from.0, from.1));
                            path.line_to(at(to.0, to.1));
                            if let Ok(path) = path.build() {
                                window.paint_path(path, line_color);
                            }
                        }
                        for ((from, to, y), (_, bottom)) in arcs.iter().zip(&arc_bottoms) {
                            let mut path = PathBuilder::stroke(px(1.5));
                            path.move_to(at(*from, *bottom));
                            path.line_to(at(*from, *y));
                            path.line_to(at(*to, *y));
                            path.line_to(at(*to, *bottom));
                            if let Ok(path) = path.build() {
                                window.paint_path(path, arc_color);
                            }
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(boxes)
            .children(arc_labels)
    }

    fn render_gates(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .gap_2()
            .p_2()
            .children(snapshot.gates.iter().enumerate().map(|(index, gate)| {
                let key = gate.key.clone();
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().status().warning_border)
                    .bg(colors.surface_background)
                    .child(Label::new(format!("Gate at {}", gate.key.node)))
                    .child(Label::new(gate.question.clone()))
                    .children(gate.result.as_ref().map(|result| {
                        Label::new(result.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(("run-view-approve", index), "Approve")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Err(message) = this.approve_gate(&key, cx) {
                                            log::warn!(
                                                "workflows: approve from the run view: {message}"
                                            );
                                        }
                                    })),
                            )
                            .child(Button::new(("run-view-revise", index), "Revise").on_click(
                                cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(Box::new(workflow::Revise), cx)
                                }),
                            )),
                    )
            }))
    }

    fn render_detail(&self, snapshot: &Snapshot) -> impl IntoElement {
        let Some(detail) = &snapshot.detail else {
            return v_flex();
        };
        let section = |title: &str, text: String| {
            v_flex()
                .child(
                    Label::new(title.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(text).size(LabelSize::Small))
        };
        let mut pane = v_flex().gap_1().p_2().child(
            Label::new(format!(
                "{} ({}, {})",
                detail.name, detail.kind, detail.status
            ))
            .size(LabelSize::Large),
        );
        if let Some(prompt) = &detail.prompt {
            pane = pane.child(section("Prompt", prompt.clone()));
        }
        if let Some(script) = &detail.script {
            pane = pane.child(section("Script", script.clone()));
        }
        if let Some(result) = &detail.result {
            pane = pane.child(section("Result", result.clone()));
        }
        if !detail.permissions.is_empty() {
            pane = pane.child(section("Permissions", detail.permissions.join("\n")));
        }
        if !detail.caps.is_empty() {
            pane = pane.child(section("Caps used", detail.caps.join(", ")));
        }
        if !detail.log.is_empty() {
            pane = pane.child(section("Log", detail.log.join("\n")));
        }
        pane
    }
}

const GRAPH_COLUMN: f32 = 190.0;
const GRAPH_ROW: f32 = 64.0;
const GRAPH_ARC: f32 = 26.0;
const GRAPH_BOX_WIDTH: f32 = 150.0;
const GRAPH_BOX_HEIGHT: f32 = 44.0;

fn box_origin(node: &LayoutNode) -> (f32, f32) {
    (
        node.column as f32 * GRAPH_COLUMN,
        node.lane as f32 * GRAPH_ROW,
    )
}

/// The segments joining boxes along forward edges: right edge of the source to the left edge of
/// the target, for every pair of boxes of the two nodes.
fn graph_lines(graph: &GraphLayout) -> Vec<((f32, f32), (f32, f32))> {
    let mut lines = Vec::new();
    for edge in &graph.edges {
        for from in graph.nodes.iter().filter(|node| node.name == edge.from) {
            for to in graph.nodes.iter().filter(|node| node.name == edge.to) {
                let (fx, fy) = box_origin(from);
                let (tx, ty) = box_origin(to);
                lines.push((
                    (fx + GRAPH_BOX_WIDTH, fy + GRAPH_BOX_HEIGHT / 2.0),
                    (tx, ty + GRAPH_BOX_HEIGHT / 2.0),
                ));
            }
        }
    }
    lines
}

/// The x of the middle of the boxes an arc leaves and enters.
fn arc_span(
    graph: &GraphLayout,
    arc: &bongos_workflow_model::layout::LayoutArc,
) -> Option<(f32, f32)> {
    let from = graph.nodes.iter().find(|node| node.name == arc.from)?;
    let to = graph.nodes.iter().find(|node| node.name == arc.to)?;
    Some((
        box_origin(from).0 + GRAPH_BOX_WIDTH / 2.0,
        box_origin(to).0 + GRAPH_BOX_WIDTH / 2.0,
    ))
}

/// The x and the bottom y of the box an arc leaves.
fn arc_ends(
    graph: &GraphLayout,
    arc: &bongos_workflow_model::layout::LayoutArc,
) -> Option<(f32, f32)> {
    let from = graph.nodes.iter().find(|node| node.name == arc.from)?;
    let (x, y) = box_origin(from);
    Some((x + GRAPH_BOX_WIDTH / 2.0, y + GRAPH_BOX_HEIGHT))
}

/// The height an arc runs at, one step lower for each arc.
fn arc_y(graph: &GraphLayout, index: usize) -> f32 {
    let lanes = graph
        .nodes
        .iter()
        .map(|node| node.lane + 1)
        .max()
        .unwrap_or(1);
    GRAPH_ROW * lanes as f32 + GRAPH_ARC * index as f32
}

fn status_color(status: RunStatus) -> Color {
    match status {
        RunStatus::Succeeded => Color::Success,
        RunStatus::Failed => Color::Error,
        RunStatus::WaitingOnYou | RunStatus::Halted => Color::Warning,
        RunStatus::Cancelled => Color::Muted,
        _ => Color::Default,
    }
}

fn box_color(status: Status) -> Color {
    match status {
        Status::Succeeded | Status::Reached => Color::Success,
        Status::Failed => Color::Error,
        Status::Waiting => Color::Warning,
        Status::Running => Color::Accent,
        Status::Pending | Status::Skipped => Color::Disabled,
    }
}

fn box_border(status: Status, cx: &App) -> Hsla {
    let theme = cx.theme();
    match status {
        Status::Succeeded | Status::Reached => theme.status().success_border,
        Status::Failed => theme.status().error_border,
        Status::Waiting => theme.status().warning_border,
        Status::Running => theme.status().info_border,
        Status::Pending | Status::Skipped => theme.colors().border_disabled,
    }
}

impl Focusable for RunView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RunView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return v_flex()
                .id("run-view")
                .key_context("RunView")
                .track_focus(&self.focus_handle)
                .size_full()
                .p_4()
                .child(Label::new("The run is no longer known").color(Color::Muted))
                .into_any_element();
        };
        let nodes = match self.mode {
            Mode::List => self.render_list(&snapshot, cx).into_any_element(),
            Mode::Graph => self.render_graph(&snapshot, cx).into_any_element(),
        };
        v_flex()
            .id("run-view")
            .key_context("RunView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &workflow::ToggleGraph, _, cx| {
                this.toggle_graph(cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.select_offset(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.select_offset(-1, cx)),
            )
            .size_full()
            .overflow_y_scroll()
            .child(self.render_header(&snapshot, cx))
            .child(nodes)
            .child(self.render_gates(&snapshot, cx))
            .children((!snapshot.outcomes.is_empty()).then(|| {
                v_flex()
                    .p_2()
                    .child(Label::new("Outcomes").color(Color::Muted))
                    .children(
                        snapshot
                            .outcomes
                            .iter()
                            .map(|outcome| Label::new(outcome.clone())),
                    )
            }))
            .child(self.render_detail(&snapshot))
            .into_any_element()
    }
}

impl Item for RunView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.snapshot {
            Some(snapshot) => format!("Run: {}", snapshot.workflow).into(),
            None => "Run".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// Shows `run` in the workspace: its open view when there is one, else a new item in the active
/// pane.
pub fn show(
    engine: WorkflowEngine,
    run: RunId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<RunView>(cx)
        .find(|view| view.read(cx).run == run);
    if let Some(view) = existing {
        view.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    watch_engine(cx);
    let view = cx.new(|cx| RunView::new(engine, run, cx));
    cx.default_global::<RunViews>().0.push(view.downgrade());
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// The run of the active run view of `workspace`, if it is showing one.
pub fn active_run(workspace: &Workspace, cx: &App) -> Option<RunId> {
    workspace
        .active_item_as::<RunView>(cx)
        .map(|view| view.read(cx).run.clone())
}

/// Registers, once, the observer that refreshes every open view when the engine publishes.
fn watch_engine<T>(cx: &mut Context<T>) {
    if cx.has_global::<RunViews>() {
        return;
    }
    cx.set_global(RunViews::default());
    WorkflowEngine::observe(cx, |_, cx| {
        let views = std::mem::take(&mut cx.default_global::<RunViews>().0);
        let live: Vec<_> = views
            .into_iter()
            .filter(|view| view.update(cx, |view, cx| view.refresh(cx)).is_ok())
            .collect();
        cx.default_global::<RunViews>().0.extend(live);
    });
}

/// The file `workflow: dump run view` writes in the logs directory.
pub const DUMP_FILE: &str = "workflow-run-view.txt";

/// Writes the text of `view` to `<dir>/workflow-run-view.txt`.
pub fn dump(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(DUMP_FILE);
    fs::write(&path, text)?;
    Ok(path)
}

/// The toast when there is no log to open yet.
pub const NO_LOG: &str = "The run has no log yet";

/// The file `workflow: open logs` opens: the log of the selected node's last try, else of the
/// last try that started.
fn log_target(engine: &WorkflowEngine, run: &str, selected: Option<&Selection>) -> Option<PathBuf> {
    let lifecycle = engine.lifecycle();
    let state = lifecycle.state(run)?;
    let attempt = lifecycle.current_attempt_dir(run)?;
    let matches = |key: &NodeKey| {
        selected.is_none_or(|(node, branch)| &key.node == node && &key.branch == branch)
    };
    let running = state
        .running
        .iter()
        .filter(|(key, _)| matches(key))
        .map(|(key, node)| (key.clone(), node.try_number))
        .next_back();
    let finished = state
        .finished
        .iter()
        .rfind(|finished| matches(&finished.key))
        .or_else(|| state.finished.last())
        .map(|finished| (finished.key.clone(), finished.try_number));
    let (key, try_number) = running.or(finished)?;
    log_file(&attempt, &key, try_number)
}

pub(crate) fn register(workspace: &mut Workspace) {
    on::<workflow::ToggleGraph>(workspace, |_, workspace, _, cx| {
        match workspace.active_item_as::<RunView>(cx) {
            Some(view) => view.update(cx, |view, cx| view.toggle_graph(cx)),
            None => show_toast(workspace, "No run view is open".to_string(), cx),
        }
    });
    on_run::<workflow::ShowRun>(workspace, |engine, run, workspace, window, cx| {
        show(engine, run, workspace, window, cx);
    });
    on_run::<workflow::OpenRunWorktree>(workspace, |engine, run, workspace, window, cx| {
        let path = engine
            .lifecycle()
            .worktree(&run)
            .map(|worktree| worktree.path.clone());
        match path {
            Some(path) => open_worktree(path, window, cx),
            None => show_toast(workspace, "The run has no worktree".to_string(), cx),
        }
    });
    on_run::<workflow::OpenLogs>(workspace, |engine, run, workspace, window, cx| {
        let selected = workspace
            .active_item_as::<RunView>(cx)
            .and_then(|view| view.read(cx).selected.clone());
        let Some(path) = log_target(&engine, &run, selected.as_ref()) else {
            show_toast(workspace, NO_LOG.to_string(), cx);
            return;
        };
        workspace
            .open_abs_path(
                path,
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..OpenOptions::default()
                },
                window,
                cx,
            )
            .detach_and_log_err(cx);
    });
    on_run::<workflow::ShowRunDiff>(workspace, show_diff);
    on_run::<workflow::DumpRunView>(workspace, |engine, run, workspace, _, cx| {
        if !cfg!(debug_assertions) {
            show_toast(
                workspace,
                "The run view dump is for debug builds".to_string(),
                cx,
            );
            return;
        }
        let text = match workspace.active_item_as::<RunView>(cx) {
            Some(view) => view.read(cx).text(),
            None => match RunData::of(&engine, &run, 0) {
                Some(data) => Snapshot::build(&data, None).text(Mode::List),
                None => "The run is no longer known".to_string(),
            },
        };
        let written = cx.background_spawn(async move { dump(paths::logs_dir(), &text) });
        cx.spawn(async move |workspace, cx| {
            let result = written.await;
            workspace
                .update(cx, |workspace, cx| {
                    let message = match result {
                        Ok(path) => format!("Wrote {}", path.display()),
                        Err(error) => format!("Could not write the run view: {error}"),
                    };
                    show_toast(workspace, message, cx)
                })
                .ok();
        })
        .detach();
    });
}

/// `workflow: show run diff`: `git diff <task branch>...<run branch>` in a read-only buffer.
fn show_diff(
    engine: WorkflowEngine,
    run: RunId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (dir, task_branch, run_branch) = {
        let lifecycle = engine.lifecycle();
        (
            lifecycle
                .worktree(&run)
                .map(|worktree| worktree.path.clone()),
            lifecycle
                .task_checkout(&run)
                .and_then(|checkout| checkout.branch.clone()),
            lifecycle
                .worktree(&run)
                .map(|worktree| worktree.branch.clone()),
        )
    };
    let (Some(dir), Some(task_branch), Some(run_branch)) = (dir, task_branch, run_branch) else {
        show_toast(
            workspace,
            "The run has no branch to compare with its task".to_string(),
            cx,
        );
        return;
    };
    let range = format!("{task_branch}...{run_branch}");
    let title = format!("Run diff {range}");
    let output = cx.background_spawn(async move {
        Git::new("git")
            .run(&dir, &["diff", "--no-color", &range])
            .await
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let result = output.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Ok(diff) if diff.is_empty() => {
                    show_toast(workspace, "The run changed nothing yet".to_string(), cx)
                }
                Ok(diff) => open_report(title, diff, workspace, window, cx),
                Err(error) => show_toast(workspace, format!("git diff failed: {error}"), cx),
            })
            .ok();
    })
    .detach();
}

/// The run `task: focus` on `task` opens a run view of: the task's active run, if it has one.
pub fn run_to_show(engine: &WorkflowEngine, task: Option<&str>) -> Option<RunId> {
    engine.active_run_id(task?)
}

/// Opens the run view of a focused task's active run in the window's active workspace. Does
/// nothing while no run is active, and while no window is.
fn open_for_focus(engine: &WorkflowEngine, task: &str, cx: &mut App) {
    let Some(run) = run_to_show(engine, Some(task)) else {
        return;
    };
    let Some(window) = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
    else {
        return;
    };
    let engine = engine.clone();
    // `task: focus` records the focus once the window switched to the task's workspace, so the
    // active workspace is the one to show the run in; the update runs after the focus update.
    cx.defer(move |cx| {
        window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace().clone();
                workspace.update(cx, |workspace, cx| show(engine, run, workspace, window, cx));
            })
            .ok();
    });
}

/// Makes `task: focus` on a leaf with an active run open its run view (PRD 8): observes the focus
/// of `bongos_work`, which exists once that crate is initialized, and shows the run when the
/// focused task changes. Quietly does nothing when the focus is not there.
pub(crate) fn observe_focus(engine: &WorkflowEngine, cx: &mut App) {
    let Some(focus) = Focus::global(cx) else {
        return;
    };
    let engine = engine.clone();
    let last = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    cx.observe(&focus, move |focus, cx| {
        let task = focus.read(cx).task_id().map(str::to_string);
        if *last.borrow() == task {
            return;
        }
        *last.borrow_mut() = task.clone();
        if let Some(task) = task {
            open_for_focus(&engine, &task, cx);
        }
    })
    .detach();
}
