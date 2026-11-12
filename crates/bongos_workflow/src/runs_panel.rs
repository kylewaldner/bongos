//! The Runs panel: the runs of every task in the bottom dock, key context `RunsPanel`. Each
//! row says the state, the task, the todo, the nodes now running or at a gate, the time, the cost
//! and the outcomes. The task, project and state filters cycle with `t`, `p` and `s` (`c` clears
//! them); `enter` opens the run view. The panel exists only while `workflows.enabled` is true:
//! [`init_panel`] adds and removes it as the setting changes. What the rows hold and how they are
//! filtered and sorted is [`RunRow`] and [`Filters`], which need no window.

use std::path::Path;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Pixels, Render, Styled as _, Subscription, WeakEntity,
    Window, div, px,
};
use settings::{Settings as _, SettingsStore};
use ui::{Color, IconName, Label, LabelCommon as _, LabelSize, h_flex, prelude::*, v_flex};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::WorkflowsSettings;
use crate::actions::{OFF_MESSAGE, show_toast, workflow};
use crate::engine::WorkflowEngine;
use crate::lifecycle::{Lifecycle, RunId, RunStatus};
use crate::outcomes::{self, RunOutcome};
use crate::run_view::{format_cost, format_ms};
use crate::runtime::Runner;

/// The key under which the dock remembers the panel.
const PANEL_KEY: &str = "RunsPanel";

/// One run in the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub run: RunId,
    pub task_id: String,
    /// The slug of the task.
    pub task: String,
    /// The worktree directory of the task, or an empty string when it has none.
    pub project: String,
    pub workflow: String,
    pub todo: String,
    pub status: RunStatus,
    /// The nodes running or at a gate, in order, without repeats.
    pub nodes: Vec<String>,
    pub elapsed_ms: u64,
    pub cost_usd: f64,
    /// One chip per outcome the run reported, such as `pr`.
    pub outcomes: Vec<String>,
}

impl RunRow {
    /// The rows of every run `lifecycle` knows, in start order.
    pub fn all<R: Runner>(lifecycle: &Lifecycle<R>) -> Vec<Self> {
        lifecycle
            .all_runs()
            .iter()
            .filter_map(|run| Self::of(lifecycle, run))
            .collect()
    }

    pub fn of<R: Runner>(lifecycle: &Lifecycle<R>, run: &str) -> Option<Self> {
        let state = lifecycle.state(run)?;
        let record = lifecycle.record(run)?;
        let checkout = lifecycle.task_checkout(run);
        let mut nodes: Vec<String> = Vec::new();
        for key in state.running.keys().chain(state.open_gates.keys()) {
            if !nodes.contains(&key.node) {
                nodes.push(key.node.clone());
            }
        }
        let outcomes = lifecycle
            .runtime()
            .definition(run)
            .map(|definition| {
                outcomes::collect(definition, &state)
                    .iter()
                    .map(|reported| chip(&reported.outcome).to_string())
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            run: run.to_string(),
            task_id: record.task_id.clone(),
            task: checkout.map(|c| c.slug.clone()).unwrap_or_default(),
            project: checkout
                .and_then(|c| c.worktree.as_deref())
                .map(project_of)
                .unwrap_or_default(),
            workflow: record.workflow.clone(),
            todo: record.todo.name.clone(),
            status: RunStatus::of(&state),
            nodes,
            elapsed_ms: state.finished.iter().map(|f| f.elapsed_ms).sum(),
            cost_usd: state.cost_usd,
            outcomes,
        })
    }

    /// The row as one line, for the dump and the tests.
    pub fn text(&self) -> String {
        let mut line = format!(
            "{} | {} | {} | {} | {} | {}",
            self.status.label(),
            self.task,
            self.todo,
            if self.nodes.is_empty() {
                "-".to_string()
            } else {
                self.nodes.join(", ")
            },
            format_ms(self.elapsed_ms),
            format_cost(self.cost_usd),
        );
        for chip in &self.outcomes {
            line.push_str(&format!(" [{chip}]"));
        }
        line
    }
}

fn chip(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Pr { .. } => "pr",
        RunOutcome::Document { .. } => "document",
        RunOutcome::Tasks { .. } => "tasks",
    }
}

fn project_of(worktree: &Path) -> String {
    worktree
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// How urgent a status is: the runs that need Kyle come first.
fn rank(status: RunStatus) -> u8 {
    match status {
        RunStatus::WaitingOnYou => 0,
        RunStatus::Failed => 1,
        RunStatus::Running | RunStatus::Pausing => 2,
        RunStatus::Paused | RunStatus::Halted => 3,
        RunStatus::Queued => 4,
        RunStatus::Succeeded => 5,
        RunStatus::Cancelled => 6,
    }
}

/// The three filters of the panel. A `None` shows everything.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filters {
    /// A task slug.
    pub task: Option<String>,
    /// A project, as [`RunRow::project`] names it.
    pub project: Option<String>,
    pub state: Option<RunStatus>,
}

/// Every status, in the order the state filter cycles through them.
const STATES: [RunStatus; 9] = [
    RunStatus::WaitingOnYou,
    RunStatus::Failed,
    RunStatus::Running,
    RunStatus::Pausing,
    RunStatus::Paused,
    RunStatus::Halted,
    RunStatus::Queued,
    RunStatus::Succeeded,
    RunStatus::Cancelled,
];

/// The value after `current` in `options`; `None` after the last and from `None` the first.
fn next_value<T: PartialEq + Clone>(options: &[T], current: &Option<T>) -> Option<T> {
    match current {
        None => options.first().cloned(),
        Some(value) => {
            let at = options.iter().position(|option| option == value);
            at.and_then(|at| options.get(at + 1)).cloned()
        }
    }
}

fn distinct(rows: &[RunRow], field: impl Fn(&RunRow) -> &String) -> Vec<String> {
    let mut values: Vec<String> = rows
        .iter()
        .map(|row| field(row).clone())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort();
    values.dedup();
    values
}

impl Filters {
    pub fn matches(&self, row: &RunRow) -> bool {
        self.task.as_ref().is_none_or(|task| *task == row.task)
            && self
                .project
                .as_ref()
                .is_none_or(|project| *project == row.project)
            && self.state.is_none_or(|state| state == row.status)
    }

    /// The rows that pass, most urgent first and the newest first within a status.
    pub fn apply(&self, rows: &[RunRow]) -> Vec<RunRow> {
        let mut shown: Vec<RunRow> = rows
            .iter()
            .rev()
            .filter(|row| self.matches(row))
            .cloned()
            .collect();
        shown.sort_by_key(|row| rank(row.status));
        shown
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn cycle_task(&mut self, rows: &[RunRow]) {
        self.task = next_value(&distinct(rows, |row| &row.task), &self.task);
    }

    pub fn cycle_project(&mut self, rows: &[RunRow]) {
        self.project = next_value(&distinct(rows, |row| &row.project), &self.project);
    }

    pub fn cycle_state(&mut self) {
        self.state = next_value(&STATES, &self.state);
    }

    /// The filters as a line, or `None` while there are none.
    pub fn text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(task) = &self.task {
            parts.push(format!("task {task}"));
        }
        if let Some(project) = &self.project {
            parts.push(format!("project {project}"));
        }
        if let Some(state) = self.state {
            parts.push(format!("state {}", state.label()));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// The panel. One per workspace; every one shows the same runs.
pub struct RunsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    filters: Filters,
    selected: Option<RunId>,
    _subscription: Subscription,
}

impl EventEmitter<PanelEvent> for RunsPanel {}

impl RunsPanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            filters: Filters::default(),
            selected: None,
            _subscription: crate::status_item::observe_changes(cx, |_, cx| cx.notify()),
        }
    }

    fn all(&self, cx: &App) -> Vec<RunRow> {
        cx.try_global::<WorkflowEngine>()
            .map(|engine| RunRow::all(&engine.lifecycle()))
            .unwrap_or_default()
    }

    /// The rows shown now.
    pub fn rows(&self, cx: &App) -> Vec<RunRow> {
        self.filters.apply(&self.all(cx))
    }

    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    /// The selected run: the chosen one while it is shown, else the first row.
    pub fn selected(&self, cx: &App) -> Option<RunId> {
        let rows = self.rows(cx);
        self.selected
            .as_ref()
            .filter(|run| rows.iter().any(|row| &row.run == *run))
            .cloned()
            .or_else(|| rows.first().map(|row| row.run.clone()))
    }

    pub fn select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected(cx)
            .and_then(|run| rows.iter().position(|row| row.run == run))
            .unwrap_or(0);
        let next = (current as isize + offset).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = Some(rows[next].run.clone());
        cx.notify();
    }

    pub fn cycle_task(&mut self, cx: &mut Context<Self>) {
        let all = self.all(cx);
        self.filters.cycle_task(&all);
        cx.notify();
    }

    pub fn cycle_project(&mut self, cx: &mut Context<Self>) {
        let all = self.all(cx);
        self.filters.cycle_project(&all);
        cx.notify();
    }

    pub fn cycle_state(&mut self, cx: &mut Context<Self>) {
        self.filters.cycle_state();
        cx.notify();
    }

    pub fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.filters = Filters::default();
        cx.notify();
    }

    /// Opens the run view of the selected run.
    pub fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.selected(cx) else {
            return;
        };
        let Some(engine) = cx.try_global::<WorkflowEngine>().cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    crate::run_view::show(engine, run, workspace, window, cx)
                })
                .ok();
        });
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "t" => self.cycle_task(cx),
            "p" => self.cycle_project(cx),
            "s" => self.cycle_state(cx),
            "c" => self.clear_filters(cx),
            _ => {}
        }
    }

    fn render_row(&self, row: &RunRow, selected: bool, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status_color = match row.status {
            RunStatus::WaitingOnYou => Color::Warning,
            RunStatus::Failed => Color::Error,
            RunStatus::Succeeded => Color::Success,
            RunStatus::Running | RunStatus::Pausing => Color::Accent,
            _ => Color::Muted,
        };
        let nodes = if row.nodes.is_empty() {
            "-".to_string()
        } else {
            row.nodes.join(", ")
        };
        let mut line = h_flex()
            .id(gpui::SharedString::from(format!("run-{}", row.run)))
            .w_full()
            .px_2()
            .py_1()
            .gap_3()
            .when(selected, |line| line.bg(colors.element_selected))
            .child(
                Label::new(row.status.label())
                    .size(LabelSize::Small)
                    .color(status_color),
            )
            .child(Label::new(row.task.clone()).size(LabelSize::Small))
            .child(
                Label::new(row.todo.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(nodes).size(LabelSize::Small))
            .child(Label::new(format_ms(row.elapsed_ms)).size(LabelSize::Small))
            .child(Label::new(format_cost(row.cost_usd)).size(LabelSize::Small));
        for chip in &row.outcomes {
            line = line.child(
                div()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border)
                    .child(Label::new(chip.clone()).size(LabelSize::XSmall)),
            );
        }
        line
    }
}

impl Render for RunsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let selected = self.selected(cx);
        let header = self
            .filters
            .text()
            .map(|text| format!("Filters: {text} (c clears)"))
            .unwrap_or_else(|| "Filters: none (t task, p project, s state)".to_string());
        let body = if rows.is_empty() {
            div()
                .p_3()
                .child(Label::new("No runs").color(Color::Muted))
                .into_any_element()
        } else {
            v_flex()
                .children(rows.iter().map(|row| {
                    let is_selected = selected.as_ref() == Some(&row.run);
                    self.render_row(row, is_selected, cx)
                }))
                .into_any_element()
        };
        v_flex()
            .id("runs-panel")
            .key_context("RunsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.select_offset(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.select_offset(-1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.open_selected(window, cx)),
            )
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .child(
                div().px_2().py_1().child(
                    Label::new(header)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(body)
    }
}

impl Focusable for RunsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for RunsPanel {
    fn persistent_name() -> &'static str {
        "Runs Panel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PlayOutlined)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Runs Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(workflow::ShowRuns)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// Adds the Runs panel to `workspace` while `workflows.enabled` is true and removes it while
/// false, now and whenever the settings change.
pub fn init_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    sync_panel(workspace, window, cx);
    cx.observe_global_in::<SettingsStore>(window, |workspace, window, cx| {
        sync_panel(workspace, window, cx)
    })
    .detach();
}

fn sync_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let enabled = WorkflowsSettings::get_global(cx).enabled;
    match (enabled, workspace.panel::<RunsPanel>(cx)) {
        (true, None) => {
            let weak = cx.entity().downgrade();
            let panel = cx.new(|cx| RunsPanel::new(weak, cx));
            workspace.add_panel(panel, window, cx);
        }
        (false, Some(panel)) => workspace.remove_panel(&panel, window, cx),
        _ => {}
    }
}

/// `workflow: show runs`: focuses the Runs panel, or hides it when it already has focus.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &workflow::ShowRuns, window, cx| {
        if !WorkflowsSettings::get_global(cx).enabled {
            show_toast(workspace, OFF_MESSAGE.to_string(), cx);
            return;
        }
        workspace.toggle_panel_focus::<RunsPanel>(window, cx);
    });
}
