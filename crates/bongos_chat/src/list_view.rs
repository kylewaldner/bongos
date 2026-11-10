//! The list view of the Chats panel (design D20): a header with each CLI's health and fix, the
//! scope line, then Open and Archived (collapsed until opened), each chat a row sorted by last
//! activity, newest first.
//!
//! A row shows the chat's name, agent, type, layout, when it started and was last active, its
//! worktree (shortened), its state, the chat it was forked from and how many forks it has, and
//! labels: handoff, each fallback its launch used, each skill of its type that was missing, an
//! error for a record that could not be read, and the note that the artifact list updates on the
//! next resume for a running chat whose list is stale ([`crate::ROW_NOTE`]). No row names an
//! artifact: the note says nothing about which one changed.
//!
//! Keys, in the `ChatList` context: `up` and `down` select, `enter` opens, `f` forks the chat into
//! a task, `r` renames, `a` archives, `u` restores, `/` filters by name, `t` switches between the
//! focused task and all tasks. Each is an action; the bindings come with the key bindings step.
//! The filter, the scope toggle and the state of the Archived section are saved (see
//! [`crate::view_state`]).
//!
//! The view reads the records, the runtime and the health, and writes nothing to a record; the
//! commands behind the keys do their own writing.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use bongos_chat_model::{ChatEntry, ChatRecord, Fork, Layout, timestamp_utc};
use bongos_work::{Focus, WorkStore, WorkStoreEvent, ask, focused};
use bongos_work_model::Timestamp;
use bongos_work_model::store::WorkTree;
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window,
};
use ui::{
    Button, Clickable as _, Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    ListItem, ListItemSpacing, StatefulInteractiveElement as _, Toggleable as _, h_flex, v_flex,
};
use workspace::Workspace;

use crate::actions::chat;
use crate::artifacts_view::{ROW_NOTE, stale_chat};
use crate::commands::{archive_and_say, ask_rename, open_chat, restore_and_say, toast_on_error};
use crate::fork::{fork_label, pick_task_and_fork};
use crate::live_modes::is_open;
use crate::send::show_toast;
use crate::view_state::{
    ALL_TASKS_KEY, ARCHIVED_EXPANDED_KEY, FILTER_KEY, read, read_flag, write, write_flag,
};
use crate::{
    ChatHealth, ChatHealthEvent, ChatRuntime, ChatRuntimeEvent, ChatStoreEvent, Health, HealthState,
};

/// What the scope line says while the list shows every task.
pub(crate) const ALL_TASKS: &str = "all tasks";

/// What the list says when nothing is listed.
pub(crate) const NO_CHATS: &str = "No chats; start one with chat: new";

/// One chat of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub id: String,
    /// A record that could not be read is listed by its file name and cannot be selected.
    pub selectable: bool,
    pub archived: bool,
    pub title: String,
    pub lines: Vec<String>,
}

/// Everything the list shows, read from the records, the runtime and the health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listing {
    pub health: Vec<String>,
    /// "billing-proration" or "all tasks".
    pub scope: String,
    pub filter: String,
    pub open: Vec<Row>,
    pub archived: Vec<Row>,
    pub archived_expanded: bool,
}

impl Listing {
    /// The rows that can be selected, in order: the open ones, then the archived ones when that
    /// section is open.
    pub(crate) fn selectable(&self) -> Vec<&Row> {
        let archived = self.archived_expanded.then_some(&self.archived[..]);
        self.open
            .iter()
            .chain(archived.into_iter().flatten())
            .filter(|row| row.selectable)
            .collect()
    }

    /// The list as plain text, `selected` marked with `>`. The same text `chat: dump view`
    /// writes.
    pub(crate) fn text(&self, selected: Option<&str>) -> String {
        let mut text = String::from("Chats\n");
        for line in &self.health {
            text.push_str(&format!("  {line}\n"));
        }
        text.push_str(&format!("Scope: {}\n", self.scope));
        if !self.filter.is_empty() {
            text.push_str(&format!("Filter: {}\n", self.filter));
        }
        text.push_str(&format!("\nOpen ({})\n", self.open.len()));
        if self.open.is_empty() {
            text.push_str(&format!("  {NO_CHATS}\n"));
        }
        push_rows(&mut text, &self.open, selected);
        if self.archived_expanded {
            text.push_str(&format!("\nArchived ({})\n", self.archived.len()));
            push_rows(&mut text, &self.archived, selected);
        } else {
            text.push_str(&format!("\nArchived ({}) collapsed\n", self.archived.len()));
        }
        text
    }
}

fn push_rows(text: &mut String, rows: &[Row], selected: Option<&str>) {
    for row in rows {
        let mark = if selected == Some(row.id.as_str()) {
            "> "
        } else {
            "  "
        };
        text.push_str(&format!("{mark}{}\n", row.title));
        for line in &row.lines {
            text.push_str(&format!("      {line}\n"));
        }
    }
}

/// One CLI's line of the header: its state, and the fix or note when there is one.
pub(crate) fn health_line(health: &Health) -> String {
    let state = match &health.state {
        HealthState::Ready { version, .. } => format!("ready ({version})"),
        HealthState::NotFound => "not found".to_string(),
        HealthState::LoggedOut => "logged out".to_string(),
        HealthState::Unchecked => "not checked".to_string(),
    };
    if health.fix.is_empty() {
        format!("{}: {state}", health.agent.as_str())
    } else {
        format!("{}: {state} - {}", health.agent.as_str(), health.fix)
    }
}

/// `path` with `home` as `~`, and only its last three directories when it is deeper than that.
pub(crate) fn short_path(path: &Path, home: &Path) -> String {
    let shown = match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    };
    let parts: Vec<&str> = shown.split('/').collect();
    if parts.len() > 4 {
        format!(".../{}", parts[parts.len() - 3..].join("/"))
    } else {
        shown
    }
}

fn layout_name(layout: Layout) -> &'static str {
    match layout {
        Layout::Dual => "dual",
        Layout::ChatOnly => "chat-only",
    }
}

/// A timestamp as `2026-11-08 10:00Z`.
fn time_text(timestamp: &Timestamp) -> String {
    timestamp_utc(timestamp)
        .map(|time| format!("{}Z", time.format("%Y-%m-%d %H:%M")))
        .unwrap_or_else(|| timestamp.to_string())
}

/// How the list names the task `id`: its goal, with its Linear identifier when it has one.
pub(crate) fn task_name(tree: &WorkTree, id: &str) -> Option<String> {
    Some(tree.task_by_id(id)?.named_goal())
}

/// Newest last activity first, ties by id. A file that could not be read has no time: it comes
/// after the readable ones, by file name.
fn display_order(a: &ChatEntry, b: &ChatEntry) -> Ordering {
    match (a.record(), b.record()) {
        (Some(a), Some(b)) => timestamp_utc(&b.last_active)
            .cmp(&timestamp_utc(&a.last_active))
            .then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.file_name().cmp(&b.file_name()),
    }
}

/// The list of the Chats panel.
pub struct ChatListView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    all_tasks: bool,
    filter: String,
    archived_expanded: bool,
    selected: Option<String>,
    /// The running chats whose artifact list is stale, as last read.
    stale: HashSet<String>,
    refreshing: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatListView {
    /// A list in the state the key value store kept.
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(runtime) = ChatRuntime::global(cx) {
            let store = runtime.read(cx).store().clone();
            subscriptions
                .push(cx.subscribe(&store, |this, _, _: &ChatStoreEvent, cx| this.changed(cx)));
            subscriptions.push(cx.subscribe(&runtime, |this, _, _: &ChatRuntimeEvent, cx| {
                this.changed(cx)
            }));
        }
        if let Some(health) = ChatHealth::global(cx) {
            subscriptions.push(cx.subscribe(&health, |_, _, _: &ChatHealthEvent, cx| cx.notify()));
        }
        if let Some(work) = WorkStore::global(cx) {
            subscriptions
                .push(cx.subscribe(&work, |this, _, _: &WorkStoreEvent, cx| this.changed(cx)));
        }
        if let Some(focus) = Focus::global(cx) {
            subscriptions.push(cx.observe(&focus, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            all_tasks: read_flag(ALL_TASKS_KEY, cx),
            filter: read(FILTER_KEY, cx).unwrap_or_default(),
            archived_expanded: read_flag(ARCHIVED_EXPANDED_KEY, cx),
            selected: None,
            stale: HashSet::new(),
            refreshing: None,
            _subscriptions: subscriptions,
        };
        this.refresh_stale(cx);
        this
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_stale(cx);
        cx.notify();
    }

    /// Looks, in the background, at which running chats have a stale artifact list. The rows show
    /// the answer once it is in.
    fn refresh_stale(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = ChatRuntime::global(cx) else {
            return;
        };
        let ids: Vec<String> = {
            let runtime = runtime.read(cx);
            runtime
                .live_ids()
                .filter(|id| is_open(runtime.state(id)))
                .map(str::to_string)
                .collect()
        };
        let checks: Vec<(String, Task<bool>)> = ids
            .into_iter()
            .map(|id| {
                let check = stale_chat(&id, cx);
                (id, check)
            })
            .collect();
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            let mut stale = HashSet::new();
            for (id, check) in checks {
                if check.await {
                    stale.insert(id);
                }
            }
            this.update(cx, |this, cx| {
                if this.stale != stale {
                    this.stale = stale;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub fn all_tasks(&self) -> bool {
        self.all_tasks
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn archived_expanded(&self) -> bool {
        self.archived_expanded
    }

    /// The chats of every task, or of the focused one again.
    pub fn toggle_all_tasks(&mut self, cx: &mut Context<Self>) {
        self.all_tasks = !self.all_tasks;
        write_flag(ALL_TASKS_KEY, self.all_tasks, cx);
        cx.notify();
    }

    /// Lists only the chats whose name contains `filter`, ignoring case; empty lists all.
    pub fn set_filter(&mut self, filter: String, cx: &mut Context<Self>) {
        self.filter = filter.trim().to_string();
        write(
            FILTER_KEY,
            (!self.filter.is_empty()).then(|| self.filter.clone()),
            cx,
        );
        cx.notify();
    }

    pub fn set_archived_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.archived_expanded == expanded {
            return;
        }
        self.archived_expanded = expanded;
        write_flag(ARCHIVED_EXPANDED_KEY, expanded, cx);
        cx.notify();
    }

    /// What the list shows now.
    pub(crate) fn listing(&self, cx: &App) -> Listing {
        let health = ChatHealth::global(cx)
            .map(|health| health.read(cx).all().iter().map(health_line).collect())
            .unwrap_or_default();
        let tree = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
        let focused_task = tree.as_ref().and_then(|tree| focused(tree, cx));
        let scope_task = focused_task.filter(|_| !self.all_tasks);
        let scope = scope_task
            .as_deref()
            .and_then(|id| task_name(tree.as_deref()?, id))
            .unwrap_or_else(|| ALL_TASKS.to_string());
        let mut listing = Listing {
            health,
            scope,
            filter: self.filter.clone(),
            open: Vec::new(),
            archived: Vec::new(),
            archived_expanded: self.archived_expanded,
        };
        let Some(runtime) = ChatRuntime::global(cx) else {
            return listing;
        };
        let runtime = runtime.read(cx);
        let store = runtime.store().read(cx);
        let mut forks: HashMap<&str, usize> = HashMap::new();
        for chat in store.index().values() {
            if let Some(parent) = chat.entry.record().and_then(|r| r.parent.as_deref()) {
                *forks.entry(parent).or_default() += 1;
            }
        }
        let needle = self.filter.to_lowercase();
        let mut chats: Vec<_> = store
            .index()
            .values()
            .filter(|chat| {
                scope_task
                    .as_deref()
                    .is_none_or(|id| chat.task_id.as_deref() == Some(id))
            })
            .filter(|chat| {
                let name = match chat.entry.record() {
                    Some(record) => record.name.clone(),
                    None => chat.entry.file_name(),
                };
                name.to_lowercase().contains(&needle)
            })
            .collect();
        chats.sort_by(|a, b| display_order(&a.entry, &b.entry));
        for chat in chats {
            let archived = chat.entry.record().is_some_and(|record| record.archived);
            let task_goal = scope_task.is_none().then(|| {
                let goal = chat
                    .task_id
                    .as_deref()
                    .and_then(|id| task_name(tree.as_deref()?, id));
                goal.unwrap_or_else(|| "no task".to_string())
            });
            let row = match chat.entry.record() {
                Some(record) => {
                    let id = record.id.as_str();
                    let live = runtime.live(id);
                    let parent = record.parent.as_deref().map(|parent| {
                        store
                            .find(parent)
                            .and_then(|found| found.entry.record())
                            .map(|found| found.name.clone())
                    });
                    let facts = RowFacts {
                        state: runtime.state(id).to_string(),
                        home: runtime.home(),
                        task_goal,
                        parent,
                        forks: forks.get(id).copied().unwrap_or(0),
                        fallbacks: live
                            .map(|live| {
                                live.fallbacks()
                                    .iter()
                                    .map(|fallback| fallback.label().to_string())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        missing_skills: live
                            .map(|live| live.missing_skills().to_vec())
                            .unwrap_or_default(),
                        stale: self.stale.contains(id) && live.is_some(),
                    };
                    record_row(record, facts)
                }
                None => error_row(&chat.entry, task_goal),
            };
            if archived {
                listing.archived.push(row);
            } else {
                listing.open.push(row);
            }
        }
        listing
    }

    /// The text of the list.
    pub fn view_text(&self, cx: &App) -> String {
        let listing = self.listing(cx);
        let selected = self.selected_in(&listing);
        listing.text(selected.as_deref())
    }

    /// The selected row: the one chosen, else the first one that can be.
    fn selected_in(&self, listing: &Listing) -> Option<String> {
        let rows = listing.selectable();
        self.selected
            .as_ref()
            .filter(|id| rows.iter().any(|row| &row.id == *id))
            .or_else(|| rows.first().map(|row| &row.id))
            .cloned()
    }

    /// The id of the selected chat.
    pub fn selected_chat(&self, cx: &App) -> Option<String> {
        self.selected_in(&self.listing(cx))
    }

    /// Selects the chat `id` (a row that is not shown is not selected).
    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.selected = Some(id.to_string());
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let listing = self.listing(cx);
        let rows = listing.selectable();
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected_in(&listing)
            .and_then(|id| rows.iter().position(|row| row.id == id))
            .unwrap_or(0);
        let next = (current as isize + by).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = Some(rows[next].id.clone());
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(-1, cx);
    }

    /// Runs `then` on the workspace once the current update is over, so the workspace is free to
    /// be updated whatever called this.
    fn with_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        cx.defer_in(window, move |_, window, cx| {
            workspace.update(cx, |workspace, cx| then(workspace, window, cx));
        });
    }

    /// Runs `then` with the selected chat; with no row to act on, lets the action go on to the
    /// workspace, which says there is no chat.
    fn with_selected(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(String, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let Some(id) = self.selected_chat(cx) else {
            cx.propagate();
            return;
        };
        self.with_workspace(window, cx, move |workspace, window, cx| {
            then(id, workspace, window, cx)
        });
    }

    /// `enter`: shows the chat, resuming it when it is closed.
    fn open_selected(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.with_selected(window, cx, |id, workspace, window, cx| {
            let opened = open_chat(&id, workspace, window, cx);
            toast_on_error(opened, cx);
        });
    }

    /// `f`: forks the chat into a task.
    fn fork_selected(&mut self, _: &chat::ForkToTask, window: &mut Window, cx: &mut Context<Self>) {
        self.with_selected(window, cx, |id, workspace, window, cx| {
            pick_task_and_fork(id, false, workspace, window, cx)
        });
    }

    /// `r`: asks for a new name.
    fn rename_selected(&mut self, _: &chat::Rename, window: &mut Window, cx: &mut Context<Self>) {
        self.with_selected(window, cx, |id, workspace, window, cx| {
            ask_rename(id, workspace, window, cx)
        });
    }

    /// `a`: moves the chat to the archive.
    fn archive_selected(&mut self, _: &chat::Archive, window: &mut Window, cx: &mut Context<Self>) {
        self.with_selected(window, cx, |id, workspace, _, cx| {
            if is_archived(&id, cx) {
                let name = crate::commands::chat_name(&id, cx);
                show_toast(workspace, format!("{name} is already archived"), cx);
            } else {
                archive_and_say(&id, cx);
            }
        });
    }

    /// `u`: moves an archived chat back to the open list.
    fn restore_selected(&mut self, _: &chat::Restore, window: &mut Window, cx: &mut Context<Self>) {
        self.with_selected(window, cx, |id, workspace, _, cx| {
            if is_archived(&id, cx) {
                restore_and_say(&id, cx);
            } else {
                let name = crate::commands::chat_name(&id, cx);
                show_toast(workspace, format!("{name} is not archived"), cx);
            }
        });
    }

    fn toggle_all_tasks_action(
        &mut self,
        _: &chat::ToggleAllTasks,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_all_tasks(cx);
    }

    fn filter_action(&mut self, _: &chat::Filter, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_filter(window, cx);
    }

    /// `/`: asks what to filter by.
    pub fn ask_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = cx.entity().downgrade();
        let current = self.filter.clone();
        self.with_workspace(window, cx, move |workspace, window, cx| {
            ask(
                workspace,
                "Filter chats by name (empty shows all)",
                &current,
                true,
                Box::new(move |text, _, _, cx| {
                    list.update(cx, |list, cx| list.set_filter(text, cx)).ok();
                }),
                window,
                cx,
            );
        });
    }

    fn render_row(
        &self,
        index: usize,
        row: &Row,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = row.id.clone();
        let item = ListItem::new(("chat-row", index))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                v_flex()
                    .w_full()
                    .child(Label::new(row.title.clone()))
                    .children(row.lines.iter().map(|line| {
                        Label::new(line.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })),
            );
        if row.selectable {
            item.on_click(cx.listener(move |this, _, _, cx| this.select(&id, cx)))
                .into_any_element()
        } else {
            item.into_any_element()
        }
    }
}

/// Whether the record of `chat_id` says it is archived.
fn is_archived(chat_id: &str, cx: &App) -> bool {
    ChatRuntime::global(cx).is_some_and(|runtime| {
        runtime
            .read(cx)
            .store()
            .read(cx)
            .find(chat_id)
            .and_then(|chat| chat.entry.record())
            .is_some_and(|record| record.archived)
    })
}

/// What a row says beyond the record.
struct RowFacts<'a> {
    state: String,
    home: &'a Path,
    /// The task's goal, when the list spans tasks.
    task_goal: Option<String>,
    /// The parent chat's name (`None` inside: the record of the parent is gone).
    parent: Option<Option<String>>,
    forks: usize,
    fallbacks: Vec<String>,
    missing_skills: Vec<String>,
    stale: bool,
}

fn record_row(record: &ChatRecord, facts: RowFacts<'_>) -> Row {
    let mut lines = vec![
        format!(
            "agent: {}  type: {}  layout: {}  state: {}",
            record.agent,
            record.chat_type,
            layout_name(record.layout),
            facts.state
        ),
        format!(
            "started: {}  last active: {}",
            time_text(&record.created),
            time_text(&record.last_active)
        ),
        format!("worktree: {}", short_path(&record.cwd, facts.home)),
    ];
    if let Some(goal) = &facts.task_goal {
        lines.push(format!("task: {goal}"));
    }
    let mut family = Vec::new();
    if let Some(parent) = &facts.parent {
        let name = parent.as_deref().unwrap_or("a chat that is gone");
        family.push(match record.fork {
            Some(fork) => format!("{} {name}", fork_label(fork)),
            None => format!("parent: {name}"),
        });
    }
    if facts.forks > 0 {
        family.push(format!("forks: {}", facts.forks));
    }
    if !family.is_empty() {
        lines.push(family.join("  "));
    }
    let mut labels = Vec::new();
    if record.fork == Some(Fork::Handoff) {
        labels.push("handoff".to_string());
    }
    labels.extend(
        facts
            .fallbacks
            .iter()
            .map(|fallback| format!("fallback: {fallback}")),
    );
    labels.extend(
        facts
            .missing_skills
            .iter()
            .map(|skill| format!("missing skill: {skill}")),
    );
    if record.known_agent().is_none() {
        labels.push(format!("error: unknown agent {}", record.agent));
    }
    if !labels.is_empty() {
        lines.push(format!("labels: {}", labels.join("; ")));
    }
    if facts.stale {
        lines.push(ROW_NOTE.to_string());
    }
    Row {
        id: record.id.clone(),
        selectable: true,
        archived: record.archived,
        title: record.name.clone(),
        lines,
    }
}

/// A record file that could not be read, listed by its name with the reason.
fn error_row(entry: &ChatEntry, task_goal: Option<String>) -> Row {
    let mut lines = Vec::new();
    if let Some(goal) = task_goal {
        lines.push(format!("task: {goal}"));
    }
    let reason = entry
        .chat
        .as_ref()
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    lines.push(format!("labels: error: {reason}"));
    Row {
        id: entry.id(),
        selectable: false,
        archived: false,
        title: entry.file_name(),
        lines,
    }
}

impl Focusable for ChatListView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ChatListView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let listing = self.listing(cx);
        let selected = self.selected_in(&listing);
        let mut index = 0;
        let mut rows = |rows: &[Row], this: &Self, cx: &mut Context<Self>| -> Vec<AnyElement> {
            rows.iter()
                .map(|row| {
                    index += 1;
                    let is_selected = selected.as_deref() == Some(row.id.as_str());
                    this.render_row(index, row, is_selected, cx)
                })
                .collect()
        };
        let open_rows = rows(&listing.open, self, cx);
        let archived_rows = if listing.archived_expanded {
            rows(&listing.archived, self, cx)
        } else {
            Vec::new()
        };
        let expanded = listing.archived_expanded;
        let chevron = if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        let mut body = v_flex()
            .id("chat-list")
            .key_context("ChatList")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::fork_selected))
            .on_action(cx.listener(Self::rename_selected))
            .on_action(cx.listener(Self::archive_selected))
            .on_action(cx.listener(Self::restore_selected))
            .on_action(cx.listener(Self::toggle_all_tasks_action))
            .on_action(cx.listener(Self::filter_action))
            .size_full()
            .gap_1()
            .p_2()
            .overflow_y_scroll();
        for line in &listing.health {
            body = body.child(Label::new(line.clone()).size(LabelSize::Small));
        }
        body =
            body.child(Label::new(SharedString::from(listing.scope.clone())).color(Color::Accent));
        if !listing.filter.is_empty() {
            body = body.child(
                Label::new(format!("Filter: {}", listing.filter))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        body = body.child(Label::new(format!("Open ({})", listing.open.len())));
        if listing.open.is_empty() {
            body = body.child(
                Label::new(NO_CHATS)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        body = body.children(open_rows).child(
            h_flex().child(
                Button::new(
                    "chat-archived-toggle",
                    format!("Archived ({})", listing.archived.len()),
                )
                .start_icon(Icon::new(chevron).size(IconSize::Small))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_archived_expanded(!expanded, cx)),
                ),
            ),
        );
        body.children(archived_rows)
    }
}
