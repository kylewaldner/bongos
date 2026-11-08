//! What the Work panel remembers and how the owner narrows it: the filters (a todo name, a kind,
//! whether the Stale and Closed sections are open), the list or graph view, and the saved state
//! (D20). The saved state lives in the key value store under `bongos_work`, one JSON value per
//! key: `drill_path`, `expanded`, `filters` and `view` (the focused task is kept by
//! [`crate::focus`]). It is written when it changes and read once, when a panel is made.

use bongos_work_model::commands::kind_name;
use bongos_work_model::derive;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Kind, TodoState};
use db::kvp::KeyValueStore;
use gpui::{App, Context, Window};
use serde_json::{Value, json};
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::work;
use crate::commands::{ready, show_toast};
use crate::focus::KVP_NAMESPACE;
use crate::modals;
use crate::panel::with_panel;
use crate::view::Key;

const DRILL_PATH_KEY: &str = "drill_path";
const EXPANDED_KEY: &str = "expanded";
const FILTERS_KEY: &str = "filters";
const VIEW_KEY: &str = "view";

/// The kinds a filter can pick, in the order the picker lists them.
pub(crate) const KINDS: [Kind; 6] = [
    Kind::PrShip,
    Kind::PrMerge,
    Kind::Document,
    Kind::Spike,
    Kind::Babysit,
    Kind::Other,
];

/// How the panel narrows the list. A task passes when it is a leaf of the kind and with the open
/// todo asked for, or a group with such a leaf below it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// The name of the current todo a leaf must have.
    pub todo: Option<String>,
    pub kind: Option<Kind>,
    /// Whether the Stale section of the top level lists its items.
    pub show_stale: bool,
    /// Whether the Closed section of the top level lists its items.
    pub show_closed: bool,
}

impl Filters {
    /// Whether `task` shows under the todo and kind filters.
    pub fn passes(&self, task: &Task, tree: &WorkTree) -> bool {
        if self.todo.is_none() && self.kind.is_none() {
            return true;
        }
        if derive::shape(task, tree) == derive::Shape::Group {
            return task.children.iter().any(|child| self.passes(child, tree));
        }
        let Some(file) = &task.file else {
            return false;
        };
        self.kind.is_none_or(|kind| file.kind == kind)
            && self.todo.as_ref().is_none_or(|name| {
                derive::current_todo(task).is_some_and(|todo| todo.name == *name)
            })
    }

    fn to_json(&self) -> Value {
        json!({
            "todo": self.todo,
            "kind": self.kind.map(kind_name),
            "show_stale": self.show_stale,
            "show_closed": self.show_closed,
        })
    }

    fn from_json(value: &Value) -> Self {
        Self {
            todo: value
                .get("todo")
                .and_then(Value::as_str)
                .map(str::to_string),
            kind: value
                .get("kind")
                .and_then(Value::as_str)
                .and_then(|name| KINDS.into_iter().find(|kind| kind_name(*kind) == name)),
            show_stale: value
                .get("show_stale")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            show_closed: value
                .get("show_closed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// The two ways to show a group: its tasks as rows, or as the boxes and arrows of the graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    List,
    Graph,
}

impl ViewMode {
    fn name(self) -> &'static str {
        match self {
            ViewMode::List => "list",
            ViewMode::Graph => "graph",
        }
    }
}

/// Everything the panel keeps between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub path: Vec<Key>,
    pub expanded: Vec<Key>,
    pub filters: Filters,
    pub view: ViewMode,
}

const ACTIVITY_PREFIX: &str = "activity:";

fn key_text(key: &Key) -> Option<String> {
    match key {
        Key::Task(id) => Some(id.clone()),
        Key::Activity(slug) => Some(format!("{ACTIVITY_PREFIX}{slug}")),
        Key::Section(_) => None,
    }
}

fn key_from_text(text: &str) -> Key {
    match text.strip_prefix(ACTIVITY_PREFIX) {
        Some(slug) => Key::Activity(slug.to_string()),
        None => Key::Task(text.to_string()),
    }
}

fn keys_json(keys: &[Key]) -> Value {
    Value::Array(
        keys.iter()
            .filter_map(key_text)
            .map(Value::String)
            .collect(),
    )
}

fn keys_from_json(value: &Value) -> Vec<Key> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(key_from_text)
                .collect()
        })
        .unwrap_or_default()
}

impl Saved {
    /// The state of a panel, with `expanded` in a fixed order so two equal states compare equal.
    pub fn new<'a>(
        path: &[Key],
        expanded: impl IntoIterator<Item = &'a Key>,
        filters: &Filters,
        view: ViewMode,
    ) -> Self {
        let mut expanded: Vec<Key> = expanded
            .into_iter()
            .filter(|key| matches!(key, Key::Task(_)))
            .cloned()
            .collect();
        expanded.sort_by_key(key_text);
        Self {
            path: path.to_vec(),
            expanded,
            filters: filters.clone(),
            view,
        }
    }

    /// Reads what an earlier run saved. A missing or unreadable value is the default.
    pub fn read(cx: &App) -> Self {
        let db = KeyValueStore::global(cx);
        let scoped = db.scoped(KVP_NAMESPACE);
        let read = |key: &str| -> Option<Value> {
            let text = scoped.read(key).log_err().flatten()?;
            serde_json::from_str(&text).log_err()
        };
        let path = read(DRILL_PATH_KEY)
            .map(|value| keys_from_json(&value))
            .unwrap_or_default();
        let expanded = read(EXPANDED_KEY)
            .map(|value| keys_from_json(&value))
            .unwrap_or_default();
        let filters = read(FILTERS_KEY)
            .map(|value| Filters::from_json(&value))
            .unwrap_or_default();
        let view = match read(VIEW_KEY).as_ref().and_then(Value::as_str) {
            Some("graph") => ViewMode::Graph,
            _ => ViewMode::List,
        };
        Self::new(&path, &expanded, &filters, view)
    }

    /// Writes the values that differ from `before`.
    pub fn write_changes(&self, before: &Saved, cx: &App) {
        let mut values: Vec<(&'static str, Value)> = Vec::new();
        if self.path != before.path {
            values.push((DRILL_PATH_KEY, keys_json(&self.path)));
        }
        if self.expanded != before.expanded {
            values.push((EXPANDED_KEY, keys_json(&self.expanded)));
        }
        if self.filters != before.filters {
            values.push((FILTERS_KEY, self.filters.to_json()));
        }
        if self.view != before.view {
            values.push((VIEW_KEY, Value::String(self.view.name().to_string())));
        }
        if values.is_empty() {
            return;
        }
        let db = KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            let store = db.scoped(KVP_NAMESPACE);
            for (key, value) in values {
                store.write(key.to_string(), value.to_string()).await?;
            }
            Ok(())
        });
    }
}

/// The names of the todos in the tree, each once, in the order they first appear.
fn todo_names(tree: &WorkTree) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for task in tree.tasks() {
        let Some(file) = &task.file else {
            continue;
        };
        for todo in &file.todos {
            if todo.state == TodoState::Open && !names.contains(&todo.name) {
                names.push(todo.name.clone());
            }
        }
    }
    names
}

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &work::FilterTodo, window, cx| filter_todo(w, window, cx))
        .register_action(|w, _: &work::FilterKind, window, cx| filter_kind(w, window, cx))
        .register_action(|w, _: &work::ShowStale, _, cx| {
            with_panel(w, cx, |panel, cx| panel.toggle_stale(cx))
        })
        .register_action(|w, _: &work::ShowClosed, _, cx| {
            with_panel(w, cx, |panel, cx| panel.toggle_closed(cx))
        })
        .register_action(|w, _: &work::Graph, _, cx| toggle_graph(w, cx));
}

fn filter_todo(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let Some(panel) = workspace.panel::<crate::WorkPanel>(cx) else {
        show_toast(workspace, "The Work panel is not open".to_string(), cx);
        return;
    };
    let names = todo_names(&tree);
    let current = panel.read(cx).filters().todo.clone();
    let mut choices = vec!["Any todo".to_string()];
    choices.extend(names.iter().map(|name| format!("Todo: {name}")));
    let selected = current
        .and_then(|todo| names.iter().position(|name| *name == todo))
        .map_or(0, |index| index + 1);
    let panel = panel.downgrade();
    modals::pick(
        workspace,
        "Show only tasks whose current todo is",
        choices,
        selected,
        Box::new(move |index, _, _, cx| {
            let todo = index.checked_sub(1).and_then(|at| names.get(at).cloned());
            panel
                .update(cx, |panel, cx| panel.set_todo_filter(todo, cx))
                .ok();
        }),
        window,
        cx,
    );
}

fn filter_kind(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    let Some(panel) = workspace.panel::<crate::WorkPanel>(cx) else {
        show_toast(workspace, "The Work panel is not open".to_string(), cx);
        return;
    };
    let current = panel.read(cx).filters().kind;
    let mut choices = vec!["Any kind".to_string()];
    choices.extend(
        KINDS
            .iter()
            .map(|kind| format!("Kind: {}", kind_name(*kind))),
    );
    let selected = current
        .and_then(|kind| KINDS.iter().position(|other| *other == kind))
        .map_or(0, |index| index + 1);
    let panel = panel.downgrade();
    modals::pick(
        workspace,
        "Show only tasks of kind",
        choices,
        selected,
        Box::new(move |index, _, _, cx| {
            let kind = index.checked_sub(1).and_then(|at| KINDS.get(at).copied());
            panel
                .update(cx, |panel, cx| panel.set_kind_filter(kind, cx))
                .ok();
        }),
        window,
        cx,
    );
}

/// `work: graph`: shows the group the panel is in as a graph, or as a list again.
fn toggle_graph(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    let Some(panel) = workspace.panel::<crate::WorkPanel>(cx) else {
        show_toast(workspace, "The Work panel is not open".to_string(), cx);
        return;
    };
    let refusal = panel.update(cx, |panel, cx| panel.toggle_graph(cx));
    if let Some(message) = refusal {
        show_toast(workspace, message.to_string(), cx);
    }
}
