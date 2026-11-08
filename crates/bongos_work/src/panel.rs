//! The Work panel (D23): one level of the task tree in the left dock, with a breadcrumb above it.
//! RET on a group drills in, RET on a leaf focuses it (like `task: focus`), `work: up` goes back,
//! and `work: toggle row` (`tab`) expands a row inline. The panel exists only while `work.enabled`
//! is true (D18): [`init_panel`] adds and removes it as the setting changes.
//!
//! The list is rebuilt from the snapshot whenever the store reports a change, so it shows what a
//! `Changed`, `NeedsYouChanged`, `LiveModesChanged` or `ChangedFilesChanged` event announced. What
//! each row says is decided in [`crate::view`].

use std::collections::HashSet;
use std::sync::Arc;

use chrono::TimeDelta;
use fs::Fs;
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, ElementId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, ScrollStrategy,
    StatefulInteractiveElement as _, Styled as _, Subscription, UniformListScrollHandle,
    WeakEntity, Window, div, px, uniform_list,
};
use settings::{Settings as _, SettingsStore};
use ui::{
    Button, Clickable as _, Color, IconName, Label, LabelCommon as _, LabelSize, ListItem,
    ListItemSpacing, Tooltip, h_flex, prelude::*, v_flex,
};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::actions::work;
use crate::commands::{ready, show_toast};
use crate::filters::{Filters, Saved, ViewMode};
use crate::focus::focus_task;
use crate::registry::LiveModesRegistry;
use crate::view::{self, Inputs, Key, Level, Line, LineKind};
use crate::{WorkSettings, WorkStore, WriteError};
use bongos_work_model::Kind;

/// A number of days as a duration.
fn days(days: f64) -> TimeDelta {
    TimeDelta::try_milliseconds((days * 86_400_000.0) as i64).unwrap_or(TimeDelta::MAX)
}

/// The key under which the dock remembers the panel.
const PANEL_KEY: &str = "WorkPanel";

/// The panel. One per workspace; every one shows the same global store.
pub struct WorkPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    /// The groups drilled into, from the top level down.
    path: Vec<Key>,
    expanded: HashSet<Key>,
    selected: Option<Key>,
    filters: Filters,
    view: ViewMode,
    /// What was last written to the key value store (D20).
    saved: Saved,
    /// Added to the clock, so a test can let days pass.
    clock_skew: TimeDelta,
    level: Level,
    /// Whether the store has a snapshot to show.
    loaded: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for WorkPanel {}

impl WorkPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(store) = WorkStore::global(cx) {
            subscriptions.push(cx.subscribe(&store, |this, _, _, cx| this.rebuild(cx)));
        }
        // Read once; the path and the expanded rows are checked against the tree when it loads.
        let saved = Saved::read(cx);
        let mut this = Self {
            workspace,
            fs,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            path: saved.path.clone(),
            expanded: saved.expanded.iter().cloned().collect(),
            selected: None,
            filters: saved.filters.clone(),
            view: saved.view,
            saved,
            clock_skew: TimeDelta::zero(),
            level: Level::default(),
            loaded: false,
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    /// Reads the store and builds the level again, keeping the selection when its row is still
    /// there.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(store) = WorkStore::global(cx) else {
            self.loaded = false;
            self.level = Level::default();
            cx.notify();
            return;
        };
        let app: &App = cx;
        let settings = WorkSettings::get_global(app);
        let (fade, stale) = (
            days(settings.fade_after_days),
            days(settings.stale_after_days),
        );
        let now = chrono::Utc::now() + self.clock_skew;
        let level = {
            let store = store.read(app);
            match store.snapshot().filter(|_| store.is_enabled()) {
                Some(tree) => {
                    let live = |id: &str| LiveModesRegistry::live_modes(id, app);
                    let changed = |id: &str| store.changed_files(id);
                    Some(view::build(
                        &self.path,
                        &Inputs {
                            tree: &tree,
                            needs_you: store.needs_you(),
                            rebase: store.rebase_states(),
                            live_modes: &live,
                            changed_files: &changed,
                            now,
                            expanded: &self.expanded,
                            filters: &self.filters,
                            fade,
                            stale,
                            graph: self.view == ViewMode::Graph,
                        },
                    ))
                }
                None => None,
            }
        };
        self.loaded = level.is_some();
        self.level = level.unwrap_or_default();
        if self.loaded {
            // What no longer exists is dropped without a word.
            self.path = self.level.path.clone();
            if let Some(tree) = store.read(app).snapshot() {
                self.expanded.retain(|key| match key {
                    Key::Task(id) => tree.task_by_id(id).is_some(),
                    _ => false,
                });
            }
            self.persist(cx);
        }
        let rows = self.level.rows();
        if !self.selected.as_ref().is_some_and(|key| rows.contains(key)) {
            self.selected = rows.first().cloned();
        }
        self.scroll_to_selected();
        cx.notify();
    }

    /// Writes what changed since the last write (D20).
    fn persist(&mut self, cx: &App) {
        let now = Saved::new(&self.path, &self.expanded, &self.filters, self.view);
        if now != self.saved {
            now.write_changes(&self.saved, cx);
            self.saved = now;
        }
    }

    pub(crate) fn filters(&self) -> &Filters {
        &self.filters
    }

    pub(crate) fn set_todo_filter(&mut self, todo: Option<String>, cx: &mut Context<Self>) {
        self.filters.todo = todo;
        self.rebuild(cx);
    }

    pub(crate) fn set_kind_filter(&mut self, kind: Option<Kind>, cx: &mut Context<Self>) {
        self.filters.kind = kind;
        self.rebuild(cx);
    }

    /// `work: show stale`: opens or folds the Stale section of the top level.
    pub(crate) fn toggle_stale(&mut self, cx: &mut Context<Self>) {
        self.filters.show_stale = !self.filters.show_stale;
        self.rebuild(cx);
    }

    /// `work: show closed`: opens or folds the Closed section of the top level.
    pub(crate) fn toggle_closed(&mut self, cx: &mut Context<Self>) {
        self.filters.show_closed = !self.filters.show_closed;
        self.rebuild(cx);
    }

    /// `work: graph`: draws the group the panel is in as a graph, or lists it again. The reason
    /// it cannot, when it cannot.
    pub(crate) fn toggle_graph(&mut self, cx: &mut Context<Self>) -> Option<&'static str> {
        if self.view == ViewMode::Graph {
            self.view = ViewMode::List;
        } else if matches!(self.path.last(), Some(Key::Task(_))) {
            self.view = ViewMode::Graph;
        } else {
            return Some("The graph shows the tasks of one group; open a group first");
        }
        self.rebuild(cx);
        None
    }

    /// Lets time pass for the freshness of the rows.
    #[cfg(test)]
    pub(crate) fn skew_clock(&mut self, by: TimeDelta, cx: &mut Context<Self>) {
        self.clock_skew = by;
        self.rebuild(cx);
    }

    /// The titles of the dimmed rows.
    #[cfg(test)]
    pub(crate) fn faded_titles(&self) -> Vec<String> {
        self.level
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Title && line.faded)
            .map(|line| line.text.clone())
            .collect()
    }

    /// What the panel keeps between runs, as it stands now.
    #[cfg(test)]
    pub(crate) fn state(&self) -> Saved {
        Saved::new(&self.path, &self.expanded, &self.filters, self.view)
    }

    /// Whether the level is drawn as a graph.
    #[cfg(test)]
    pub(crate) fn shows_graph(&self) -> bool {
        self.level.graph
    }

    fn scroll_to_selected(&self) {
        let Some(selected) = &self.selected else {
            return;
        };
        if let Some(index) =
            self.level.lines.iter().position(|line| {
                line.kind == LineKind::Title && line.key.as_ref() == Some(selected)
            })
        {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    /// Asks the store for fresh changed-file counts of the leaves shown.
    fn refresh_changes(&self, cx: &mut Context<Self>) {
        let Some(store) = WorkStore::global(cx) else {
            return;
        };
        let ids: Vec<String> = self
            .level
            .groups
            .iter()
            .filter_map(|(key, group)| match key {
                Key::Task(id) if !group => Some(id.clone()),
                _ => None,
            })
            .collect();
        store.update(cx, |store, cx| {
            for id in &ids {
                store.refresh_changed_files(id, cx);
            }
        });
    }

    /// The breadcrumb and every line of the level, one per line of text; the selected row is
    /// marked with `>`.
    pub fn view_text(&self) -> String {
        let mut text = self.level.breadcrumb();
        for line in &self.level.lines {
            let selected = line.kind == LineKind::Title && line.key == self.selected;
            text.push('\n');
            text.push_str(match (selected, line.faded) {
                (true, _) => ">",
                (false, true) => "~",
                (false, false) => " ",
            });
            text.push_str(&line.plain());
        }
        text.push('\n');
        text
    }

    /// The text of every line of the level, without the breadcrumb.
    pub fn lines(&self) -> Vec<String> {
        self.level.lines.iter().map(Line::plain).collect()
    }

    /// The breadcrumb, such as `Work > Invoicing > Phase`.
    pub fn breadcrumb(&self) -> String {
        self.level.breadcrumb()
    }

    /// Goes into the group `key`.
    fn drill(&mut self, key: Key, cx: &mut Context<Self>) {
        self.path.push(key);
        self.selected = None;
        self.rebuild(cx);
        self.refresh_changes(cx);
    }

    /// Shows the level `depth` keys down the path (0 is the top level).
    fn show_depth(&mut self, depth: usize, cx: &mut Context<Self>) {
        if depth >= self.path.len() {
            return;
        }
        let left = self.path[depth].clone();
        self.path.truncate(depth);
        self.selected = Some(left);
        self.rebuild(cx);
        self.refresh_changes(cx);
    }

    /// `work: up`: the group the level is in becomes the level, with the group selected.
    pub fn up(&mut self, cx: &mut Context<Self>) {
        let depth = self.path.len();
        if depth > 0 {
            self.show_depth(depth - 1, cx);
        }
    }

    /// `work: toggle row`: expands the selected row, or folds it.
    pub fn toggle_row(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        if matches!(key, Key::Section(_)) {
            self.activate_section(&key, cx);
            return;
        }
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.rebuild(cx);
    }

    fn select(&mut self, key: Key, cx: &mut Context<Self>) {
        self.selected = Some(key);
        self.scroll_to_selected();
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
            .and_then(|key| rows.iter().position(|row| row == key));
        let next = match current {
            Some(index) => index.saturating_add_signed(offset).min(rows.len() - 1),
            None => 0,
        };
        self.select(rows[next].clone(), cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_offset(1, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_offset(-1, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.level.rows().first() {
            self.select(key.clone(), cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.level.rows().last() {
            self.select(key.clone(), cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.selected.clone() {
            self.activate(key, window, cx);
        }
    }

    /// RET on the heading of a section folds or opens it.
    fn activate_section(&mut self, key: &Key, cx: &mut Context<Self>) {
        match key {
            Key::Section(view::Section::Stale) => self.toggle_stale(cx),
            Key::Section(view::Section::Closed) => self.toggle_closed(cx),
            _ => {}
        }
    }

    /// RET on a row: a group is drilled into, a leaf is focused, a section is folded or opened.
    fn activate(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(key, Key::Section(_)) {
            self.activate_section(&key, cx);
            return;
        }
        if self.level.is_group(&key) {
            self.drill(key, cx);
            return;
        }
        let Key::Task(id) = key else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Deferred, so the workspace is free to be updated whatever called this.
        cx.defer_in(window, move |_, window, cx| {
            workspace.update(cx, |workspace, cx| focus_task(&id, workspace, window, cx));
        });
    }

    fn render_line(&self, index: usize, line: &Line, cx: &mut Context<Self>) -> AnyElement {
        let selected = line.key.is_some() && line.key == self.selected;
        let id = ElementId::NamedInteger("work-line".into(), index as u64);
        let title_color = if line.faded {
            Color::Disabled
        } else {
            Color::Default
        };
        let quiet_color = if line.faded {
            Color::Disabled
        } else {
            Color::Muted
        };
        let content = match line.kind {
            LineKind::Title if self.level.graph => div()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(title_color),
                )
                .into_any_element(),
            LineKind::Title => h_flex()
                .w_full()
                .gap_2()
                .justify_between()
                .child(Label::new(line.text.clone()).color(title_color).truncate())
                .child(
                    Label::new(line.trailing.clone())
                        .size(LabelSize::Small)
                        .color(quiet_color),
                )
                .into_any_element(),
            LineKind::Detail => div()
                .pl_3()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .color(quiet_color)
                        .truncate(),
                )
                .into_any_element(),
            LineKind::Graph => div()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(quiet_color),
                )
                .into_any_element(),
            LineKind::Edge => div()
                .pl_2()
                .w_full()
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .italic()
                        .truncate(),
                )
                .into_any_element(),
            LineKind::Extra => {
                let label = Label::new(line.text.clone())
                    .size(LabelSize::Small)
                    .truncate();
                let element = div().pl_6().w_full().child(label);
                match line.full.clone() {
                    Some(full) => element
                        .id(ElementId::NamedInteger("work-full".into(), index as u64))
                        .tooltip(Tooltip::text(full))
                        .into_any_element(),
                    None => element.into_any_element(),
                }
            }
        };
        let item = ListItem::new(id)
            .inset(true)
            .spacing(ListItemSpacing::Dense)
            .toggle_state(selected)
            .child(content);
        match line.key.clone() {
            Some(key) => item
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.select(key.clone(), cx);
                    if event.click_count() >= 2 {
                        this.activate(key.clone(), window, cx);
                    }
                }))
                .into_any_element(),
            None => item.into_any_element(),
        }
    }

    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let last = self.level.crumbs.len().saturating_sub(1);
        let mut bar = h_flex().w_full().flex_wrap().gap_1().px_2().py_1();
        for (depth, crumb) in self.level.crumbs.iter().enumerate() {
            if depth > 0 {
                bar = bar.child(Label::new(">").size(LabelSize::Small).color(Color::Muted));
            }
            let mut button = Button::new(("work-crumb", depth), crumb.clone())
                .label_size(LabelSize::Small)
                .disabled(depth == last);
            if depth < last {
                // The crumb at `depth` is the level with `depth` keys of the path.
                button =
                    button.on_click(cx.listener(move |this, _, _, cx| this.show_depth(depth, cx)));
            }
            bar = bar.child(button);
        }
        bar
    }

    fn message(&self, text: &'static str) -> AnyElement {
        div()
            .p_3()
            .child(Label::new(text).color(Color::Muted))
            .into_any_element()
    }
}

impl Render for WorkPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if !self.loaded {
            self.message("The Work tree is not loaded")
        } else if self.level.lines.is_empty() {
            self.message("Nothing here yet")
        } else {
            uniform_list(
                "work-lines",
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
            .into_any_element()
        };
        v_flex()
            .id("work-panel")
            .key_context("WorkPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .child(self.render_breadcrumb(cx))
            .child(body)
    }
}

impl Focusable for WorkPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for WorkPanel {
    fn persistent_name() -> &'static str {
        "Work Panel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        WorkSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.work.get_or_insert_default().dock = Some(position.into());
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        px(WorkSettings::get_global(cx).default_width)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Work Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(work::TogglePanel)
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.rebuild(cx);
            self.refresh_changes(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

/// Adds the Work panel to `workspace` while `work.enabled` is true and removes it while false,
/// now and whenever the settings change (D18).
pub fn init_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    sync_panel(workspace, window, cx);
    cx.observe_global_in::<SettingsStore>(window, |workspace, window, cx| {
        sync_panel(workspace, window, cx)
    })
    .detach();
}

fn sync_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let enabled = WorkSettings::get_global(cx).enabled;
    match (enabled, workspace.panel::<WorkPanel>(cx)) {
        (true, None) => {
            let weak = cx.entity().downgrade();
            let fs = workspace.app_state().fs.clone();
            let panel = cx.new(|cx| WorkPanel::new(weak, fs, window, cx));
            workspace.add_panel(panel, window, cx);
        }
        (false, Some(panel)) => workspace.remove_panel(&panel, window, cx),
        _ => {}
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &work::TogglePanel, window, cx| toggle_panel(w, window, cx))
        .register_action(|w, _: &work::Up, _, cx| with_panel(w, cx, |panel, cx| panel.up(cx)))
        .register_action(|w, _: &work::ToggleRow, _, cx| {
            with_panel(w, cx, |panel, cx| panel.toggle_row(cx))
        });
    #[cfg(debug_assertions)]
    workspace.register_action(|w, _: &work::DumpView, _, cx| dump_view(w, cx));
}

/// `work: toggle panel`. The panel opens while the tree is still loading, so this only needs the
/// store to be on.
fn toggle_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let enabled = WorkStore::global(cx).is_some_and(|store| store.read(cx).is_enabled());
    if !enabled {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    }
    workspace.toggle_panel_focus::<WorkPanel>(window, cx);
}

/// Runs `then` on the panel once the store is on and loaded; otherwise a toast says why.
pub(crate) fn with_panel(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut WorkPanel, &mut Context<WorkPanel>),
) {
    if ready(workspace, cx).is_none() {
        return;
    }
    match workspace.panel::<WorkPanel>(cx) {
        Some(panel) => panel.update(cx, then),
        None => show_toast(workspace, "The Work panel is not open".to_string(), cx),
    }
}

/// The file `work: dump view` writes in the logs directory.
#[cfg(debug_assertions)]
pub(crate) const DUMP_FILE: &str = "work-view.txt";

/// Writes `text` to [`DUMP_FILE`] in `logs_dir` and returns the path.
#[cfg(debug_assertions)]
pub(crate) fn write_view(
    text: &str,
    logs_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(debug_assertions)]
fn dump_view(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    let Some(panel) = workspace.panel::<WorkPanel>(cx) else {
        show_toast(workspace, "The Work panel is not open".to_string(), cx);
        return;
    };
    let text = panel.read(cx).view_text();
    let message = match write_view(&text, paths::logs_dir()) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the Work view: {error}"),
    };
    show_toast(workspace, message, cx);
}
