//! The declarations: pools, queues and triggers parsed with line numbers, and the lane table fields
//! (`pulls`, `pools`, `hooks`) as raw located values. An unknown key is an error, as in the lane
//! table. A malformed field is a diagnostic at its line; the declaration is kept when its name is
//! readable, so the rules in [`crate::validate`] still see it.

use std::collections::BTreeMap;

use crate::schedule::{Schedule, parse_timer};
use crate::source::{
    Diagnostic, LineIndex, Located, ParseError, RawEntry, check_nested_keys, children, read_file,
};

const POOL_KEYS: &[&str] = &[
    "name",
    "title",
    "kind",
    "scope",
    "hold_while_waiting",
    "max_hold",
    "releasable",
    "size",
    "guard",
    "settle_seconds",
];
const GUARD_KEYS: &[&str] = &["free_memory_gb", "free_disk_gb", "path", "script"];
const QUEUE_KEYS: &[&str] = &[
    "name",
    "title",
    "item",
    "task_field",
    "max_attempts",
    "on_exhausted",
    "unattended",
];
const FIELD_KEYS: &[&str] = &["type", "optional"];
const TRIGGER_KEYS: &[&str] = &["name", "title", "on", "catch_up", "action", "unattended"];
const WORKFLOW_ACTION_KEYS: &[&str] = &["workflow", "task", "repo", "base", "merge_into", "inputs"];
const PUSH_KEYS: &[&str] = &["queue", "fields"];

/// What parsing a declaration file yields: the declarations and the problems found on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed<T> {
    pub decls: Vec<T>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Slots,
    Rw,
    Mutex,
    Guard,
}

impl PoolKind {
    pub fn word(self) -> &'static str {
        match self {
            PoolKind::Slots => "slots",
            PoolKind::Rw => "rw",
            PoolKind::Mutex => "mutex",
            PoolKind::Guard => "guard",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PoolScope {
    #[default]
    Project,
    Machine,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GuardCheck {
    FreeMemoryGb(f64),
    FreeDiskGb { gb: f64, path: Option<String> },
    Script(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Guard {
    pub line: usize,
    pub check: GuardCheck,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolDecl {
    pub name: String,
    pub line: usize,
    pub title: Option<String>,
    pub kind: PoolKind,
    pub scope: PoolScope,
    pub hold_while_waiting: bool,
    /// As written (`4h`); [`crate::parse_duration`] reads it.
    pub max_hold: Option<String>,
    pub releasable: bool,
    pub size: Option<u32>,
    pub guard: Option<Guard>,
    pub settle_seconds: Option<u32>,
    pub key_lines: BTreeMap<String, usize>,
}

impl PoolDecl {
    pub fn key_line(&self, key: &str) -> usize {
        self.key_lines.get(key).copied().unwrap_or(self.line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Number,
    Bool,
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemField {
    pub name: String,
    pub ty: FieldType,
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExhaustedAction {
    #[default]
    Notify,
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueDecl {
    pub name: String,
    pub line: usize,
    pub title: Option<String>,
    pub item: Vec<ItemField>,
    pub task_field: Option<String>,
    pub max_attempts: Option<u32>,
    pub on_exhausted: ExhaustedAction,
    pub unattended: Option<bool>,
    pub key_lines: BTreeMap<String, usize>,
}

impl QueueDecl {
    pub fn key_line(&self, key: &str) -> usize {
        self.key_lines.get(key).copied().unwrap_or(self.line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllDone {
    WorkKind(String),
    Under(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum On {
    Timer(Schedule),
    Queue {
        queue: String,
        consume: bool,
    },
    AllDone(AllDone),
    TaskDone(String),
    RunEnded {
        workflow: Option<String>,
        result: RunResult,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Workflow {
        workflow: String,
        task: Option<String>,
        repo: Option<String>,
        base: Option<String>,
        merge_into: Option<String>,
        inputs: BTreeMap<String, String>,
    },
    Push {
        queue: String,
        fields: BTreeMap<String, String>,
    },
    Dispatcher,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TriggerDecl {
    pub name: String,
    pub line: usize,
    pub title: Option<String>,
    pub on: On,
    pub catch_up: bool,
    pub action: Action,
    pub unattended: Option<bool>,
    pub key_lines: BTreeMap<String, usize>,
}

impl TriggerDecl {
    pub fn key_line(&self, key: &str) -> usize {
        self.key_lines.get(key).copied().unwrap_or(self.line)
    }
}

fn string_field(entry: &RawEntry, key: &str, out: &mut Vec<Diagnostic>) -> Option<String> {
    match entry.table.get(key)? {
        toml::Value::String(text) => Some(text.clone()),
        _ => {
            out.push(Diagnostic {
                line: entry.key_line(key),
                message: format!("{key} must be a string"),
            });
            None
        }
    }
}

fn bool_field(entry: &RawEntry, key: &str, out: &mut Vec<Diagnostic>) -> Option<bool> {
    match entry.table.get(key)? {
        toml::Value::Boolean(value) => Some(*value),
        _ => {
            out.push(Diagnostic {
                line: entry.key_line(key),
                message: format!("{key} must be true or false"),
            });
            None
        }
    }
}

fn count_field(entry: &RawEntry, key: &str, out: &mut Vec<Diagnostic>) -> Option<u32> {
    match entry.table.get(key)? {
        toml::Value::Integer(value) if u32::try_from(*value).is_ok() => u32::try_from(*value).ok(),
        _ => {
            out.push(Diagnostic {
                line: entry.key_line(key),
                message: format!("{key} must be a whole number"),
            });
            None
        }
    }
}

fn name_field(entry: &RawEntry, what: &str, out: &mut Vec<Diagnostic>) -> Option<String> {
    let name = string_field(entry, "name", out);
    if name.is_none() && !entry.table.contains_key("name") {
        out.push(Diagnostic {
            line: entry.line,
            message: format!("a {what} needs a name"),
        });
    }
    name
}

/// Parses `pools/pools.toml`. Blank text (a missing file) declares nothing.
pub fn parse_pools(text: &str) -> Result<Parsed<PoolDecl>, ParseError> {
    let file = read_file(text, "pool", POOL_KEYS, "[[pool]]")?;
    let mut out = file.diagnostics;
    let mut decls = Vec::new();
    for entry in &file.entries {
        let Some(name) = name_field(entry, "pool", &mut out) else {
            continue;
        };
        let kind = match string_field(entry, "kind", &mut out).as_deref() {
            Some("slots") => PoolKind::Slots,
            Some("rw") => PoolKind::Rw,
            Some("mutex") => PoolKind::Mutex,
            Some("guard") => PoolKind::Guard,
            Some(other) => {
                out.push(Diagnostic {
                    line: entry.key_line("kind"),
                    message: format!(
                        "pool `{name}`: kind must be slots, rw, mutex or guard, not \"{other}\""
                    ),
                });
                continue;
            }
            None => {
                if !entry.table.contains_key("kind") {
                    out.push(Diagnostic {
                        line: entry.line,
                        message: format!("pool `{name}` needs a kind (slots, rw, mutex or guard)"),
                    });
                }
                continue;
            }
        };
        let scope = match string_field(entry, "scope", &mut out).as_deref() {
            None | Some("project") => PoolScope::Project,
            Some("machine") => PoolScope::Machine,
            Some(other) => {
                out.push(Diagnostic {
                    line: entry.key_line("scope"),
                    message: format!(
                        "pool `{name}`: scope must be project or machine, not \"{other}\""
                    ),
                });
                PoolScope::Project
            }
        };
        let guard = entry
            .table
            .get("guard")
            .and_then(|value| parse_guard(&name, value, entry.key_line("guard"), &mut out));
        decls.push(PoolDecl {
            title: string_field(entry, "title", &mut out),
            kind,
            scope,
            hold_while_waiting: bool_field(entry, "hold_while_waiting", &mut out).unwrap_or(false),
            max_hold: string_field(entry, "max_hold", &mut out),
            releasable: bool_field(entry, "releasable", &mut out).unwrap_or(false),
            size: count_field(entry, "size", &mut out),
            guard,
            settle_seconds: count_field(entry, "settle_seconds", &mut out),
            key_lines: entry.key_lines.clone(),
            name,
            line: entry.line,
        });
    }
    Ok(Parsed {
        decls,
        diagnostics: out,
    })
}

fn number(value: &toml::Value) -> Option<f64> {
    match value {
        toml::Value::Integer(value) => Some(*value as f64),
        toml::Value::Float(value) => Some(*value),
        _ => None,
    }
}

fn parse_guard(
    pool: &str,
    value: &toml::Value,
    line: usize,
    out: &mut Vec<Diagnostic>,
) -> Option<Guard> {
    let mut problem = |message: String| {
        out.push(Diagnostic {
            line,
            message: format!("pool `{pool}`: {message}"),
        });
        None::<Guard>
    };
    let Some(table) = value.as_table() else {
        return problem("guard must be a table like { free_memory_gb = 4 }".to_string());
    };
    for key in table.keys() {
        if !GUARD_KEYS.contains(&key.as_str()) {
            return problem(format!("unknown key \"{key}\" in guard"));
        }
    }
    let path = match table.get("path") {
        None => None,
        Some(toml::Value::String(path)) => Some(path.clone()),
        Some(_) => return problem("guard path must be a string".to_string()),
    };
    let check = match (
        table.get("free_memory_gb"),
        table.get("free_disk_gb"),
        table.get("script"),
    ) {
        (Some(gb), None, None) => {
            if path.is_some() {
                return problem("path goes with free_disk_gb".to_string());
            }
            match number(gb) {
                Some(gb) if gb > 0.0 => GuardCheck::FreeMemoryGb(gb),
                _ => return problem("free_memory_gb must be a number above 0".to_string()),
            }
        }
        (None, Some(gb), None) => match number(gb) {
            Some(gb) if gb > 0.0 => GuardCheck::FreeDiskGb { gb, path },
            _ => return problem("free_disk_gb must be a number above 0".to_string()),
        },
        (None, None, Some(script)) => {
            if path.is_some() {
                return problem("path goes with free_disk_gb".to_string());
            }
            match script.as_str() {
                Some(script) => GuardCheck::Script(script.to_string()),
                None => return problem("script must be a definition path".to_string()),
            }
        }
        _ => {
            return problem(
                "a guard has exactly one of free_memory_gb, free_disk_gb and script".to_string(),
            );
        }
    };
    Some(Guard { line, check })
}

/// Parses `queues/queues.toml`. Blank text (a missing file) declares nothing.
pub fn parse_queues(text: &str) -> Result<Parsed<QueueDecl>, ParseError> {
    let file = read_file(text, "queue", QUEUE_KEYS, "[[queue]]")?;
    let mut out = file.diagnostics;
    let mut decls = Vec::new();
    for entry in &file.entries {
        let Some(name) = name_field(entry, "queue", &mut out) else {
            continue;
        };
        let item_line = entry.key_line("item");
        let mut item = Vec::new();
        match entry.table.get("item") {
            None => {}
            Some(toml::Value::Table(fields)) => {
                for (field, spec) in fields {
                    let Some(spec) = spec.as_table() else {
                        out.push(Diagnostic {
                            line: item_line,
                            message: format!(
                                "queue `{name}`: item field `{field}` must be a table like {{ type = \"text\" }}"
                            ),
                        });
                        continue;
                    };
                    check_nested_keys(
                        spec,
                        FIELD_KEYS,
                        &format!("item field `{field}`"),
                        item_line,
                        &mut out,
                    );
                    let ty = match spec.get("type").and_then(toml::Value::as_str) {
                        Some("text") => FieldType::Text,
                        Some("number") => FieldType::Number,
                        Some("bool") => FieldType::Bool,
                        Some("task") => FieldType::Task,
                        Some(other) => {
                            out.push(Diagnostic {
                                line: item_line,
                                message: format!(
                                    "queue `{name}`: item field `{field}` has type \"{other}\", which is not text, number, bool or task"
                                ),
                            });
                            continue;
                        }
                        None => {
                            out.push(Diagnostic {
                                line: item_line,
                                message: format!(
                                    "queue `{name}`: item field `{field}` needs a type"
                                ),
                            });
                            continue;
                        }
                    };
                    let optional = match spec.get("optional") {
                        None => false,
                        Some(toml::Value::Boolean(optional)) => *optional,
                        Some(_) => {
                            out.push(Diagnostic {
                                line: item_line,
                                message: format!(
                                    "queue `{name}`: optional of item field `{field}` must be true or false"
                                ),
                            });
                            false
                        }
                    };
                    item.push(ItemField {
                        name: field.clone(),
                        ty,
                        optional,
                    });
                }
            }
            Some(_) => out.push(Diagnostic {
                line: item_line,
                message: format!("queue `{name}`: item must be a table of fields"),
            }),
        }
        let on_exhausted = match string_field(entry, "on_exhausted", &mut out).as_deref() {
            None | Some("notify") => ExhaustedAction::Notify,
            Some("stop") => ExhaustedAction::Stop,
            Some(other) => {
                out.push(Diagnostic {
                    line: entry.key_line("on_exhausted"),
                    message: format!(
                        "queue `{name}`: on_exhausted must be notify or stop, not \"{other}\""
                    ),
                });
                ExhaustedAction::Notify
            }
        };
        decls.push(QueueDecl {
            title: string_field(entry, "title", &mut out),
            item,
            task_field: string_field(entry, "task_field", &mut out),
            max_attempts: count_field(entry, "max_attempts", &mut out),
            on_exhausted,
            unattended: bool_field(entry, "unattended", &mut out),
            key_lines: entry.key_lines.clone(),
            name,
            line: entry.line,
        });
    }
    Ok(Parsed {
        decls,
        diagnostics: out,
    })
}

/// Parses `triggers/triggers.toml`. Blank text (a missing file) declares nothing.
pub fn parse_triggers(text: &str) -> Result<Parsed<TriggerDecl>, ParseError> {
    let file = read_file(text, "trigger", TRIGGER_KEYS, "[[trigger]]")?;
    let mut out = file.diagnostics;
    let mut decls = Vec::new();
    for entry in &file.entries {
        let Some(name) = name_field(entry, "trigger", &mut out) else {
            continue;
        };
        let on = match entry.table.get("on") {
            Some(value) => parse_on(&name, value, entry.key_line("on"), &mut out),
            None => {
                out.push(Diagnostic {
                    line: entry.line,
                    message: format!("trigger `{name}` needs an `on`"),
                });
                None
            }
        };
        let action = match entry.table.get("action") {
            Some(value) => parse_action(&name, value, entry.key_line("action"), &mut out),
            None => {
                out.push(Diagnostic {
                    line: entry.line,
                    message: format!("trigger `{name}` needs an `action`"),
                });
                None
            }
        };
        let (Some(on), Some(action)) = (on, action) else {
            continue;
        };
        decls.push(TriggerDecl {
            title: string_field(entry, "title", &mut out),
            on,
            catch_up: bool_field(entry, "catch_up", &mut out).unwrap_or(false),
            action,
            unattended: bool_field(entry, "unattended", &mut out),
            key_lines: entry.key_lines.clone(),
            name,
            line: entry.line,
        });
    }
    Ok(Parsed {
        decls,
        diagnostics: out,
    })
}

/// The single entry of a table that must hold exactly one variant.
fn one_variant<'a>(
    trigger: &str,
    what: &str,
    value: &'a toml::Value,
    line: usize,
    out: &mut Vec<Diagnostic>,
) -> Option<(&'a String, &'a toml::Value)> {
    let Some(table) = value.as_table() else {
        out.push(Diagnostic {
            line,
            message: format!("trigger `{trigger}`: {what} must be a table with one variant"),
        });
        return None;
    };
    let mut entries = table.iter();
    match (entries.next(), entries.next()) {
        (Some(only), None) => Some(only),
        _ => {
            out.push(Diagnostic {
                line,
                message: format!(
                    "trigger `{trigger}`: {what} must be exactly one variant, found {}",
                    table.len()
                ),
            });
            None
        }
    }
}

fn strings_map(
    trigger: &str,
    what: &str,
    value: Option<&toml::Value>,
    line: usize,
    out: &mut Vec<Diagnostic>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(value) = value else {
        return map;
    };
    let Some(table) = value.as_table() else {
        out.push(Diagnostic {
            line,
            message: format!("trigger `{trigger}`: {what} must be a table of strings"),
        });
        return map;
    };
    for (key, value) in table {
        match value.as_str() {
            Some(text) => {
                map.insert(key.clone(), text.to_string());
            }
            None => out.push(Diagnostic {
                line,
                message: format!("trigger `{trigger}`: {what} `{key}` must be a string"),
            }),
        }
    }
    map
}

fn parse_on(
    trigger: &str,
    value: &toml::Value,
    line: usize,
    out: &mut Vec<Diagnostic>,
) -> Option<On> {
    // `consume` is the one key that sits beside a variant: `on = { queue = "q", consume = true }`.
    let mut consume = false;
    let stripped;
    let value = match value.as_table() {
        Some(table) if table.contains_key("queue") && table.contains_key("consume") => {
            match table.get("consume") {
                Some(toml::Value::Boolean(flag)) => consume = *flag,
                _ => {
                    out.push(Diagnostic {
                        line,
                        message: format!("trigger `{trigger}`: consume must be true or false"),
                    });
                    return None;
                }
            }
            let mut rest = table.clone();
            rest.remove("consume");
            stripped = toml::Value::Table(rest);
            &stripped
        }
        _ => value,
    };
    let (variant, body) = one_variant(trigger, "on", value, line, out)?;
    let mut problem = |message: String| {
        out.push(Diagnostic {
            line,
            message: format!("trigger `{trigger}`: {message}"),
        });
        None::<On>
    };
    match (variant.as_str(), body) {
        ("timer", toml::Value::Table(timer)) => match parse_timer(timer) {
            Ok(schedule) => Some(On::Timer(schedule)),
            Err(message) => problem(message),
        },
        ("timer", _) => problem("timer must be a table".to_string()),
        ("queue", toml::Value::String(queue)) => Some(On::Queue {
            queue: queue.clone(),
            consume,
        }),
        ("queue", _) => problem("queue must be a queue name".to_string()),
        ("all_done", toml::Value::Table(table)) => {
            for key in table.keys() {
                if !["work_kind", "under"].contains(&key.as_str()) {
                    return problem(format!("unknown key \"{key}\" in all_done"));
                }
            }
            match (
                table.get("work_kind").and_then(toml::Value::as_str),
                table.get("under").and_then(toml::Value::as_str),
            ) {
                (Some(kind), None) => Some(On::AllDone(AllDone::WorkKind(kind.to_string()))),
                (None, Some(task)) => Some(On::AllDone(AllDone::Under(task.to_string()))),
                _ => problem("all_done has exactly one of work_kind and under".to_string()),
            }
        }
        ("all_done", _) => problem("all_done must be a table".to_string()),
        ("task_done", toml::Value::String(task)) => Some(On::TaskDone(task.clone())),
        ("task_done", _) => problem("task_done must be a task id".to_string()),
        ("run_ended", toml::Value::Table(table)) => {
            for key in table.keys() {
                if !["workflow", "result"].contains(&key.as_str()) {
                    return problem(format!("unknown key \"{key}\" in run_ended"));
                }
            }
            let result = match table.get("result").and_then(toml::Value::as_str) {
                Some("succeeded") => RunResult::Succeeded,
                Some("failed") => RunResult::Failed,
                _ => return problem("run_ended result must be succeeded or failed".to_string()),
            };
            Some(On::RunEnded {
                workflow: table
                    .get("workflow")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string),
                result,
            })
        }
        ("run_ended", _) => problem("run_ended must be a table".to_string()),
        (other, _) => problem(format!(
            "on \"{other}\" is not timer, queue, all_done, task_done or run_ended"
        )),
    }
}

fn parse_action(
    trigger: &str,
    value: &toml::Value,
    line: usize,
    out: &mut Vec<Diagnostic>,
) -> Option<Action> {
    // A workflow action is flat: `{ workflow = "name", task = .., repo = .., inputs = .. }`.
    let wrapped;
    let value = if value
        .as_table()
        .is_some_and(|table| table.contains_key("workflow"))
    {
        wrapped = toml::Value::Table(toml::Table::from_iter([(
            "workflow".to_string(),
            value.clone(),
        )]));
        &wrapped
    } else {
        value
    };
    let (variant, body) = one_variant(trigger, "action", value, line, out)?;
    let mut problem = |message: String| {
        out.push(Diagnostic {
            line,
            message: format!("trigger `{trigger}`: {message}"),
        });
    };
    let text = |table: &toml::Table, key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    match (variant.as_str(), body) {
        ("dispatcher", toml::Value::Table(table)) if table.is_empty() => Some(Action::Dispatcher),
        ("dispatcher", _) => {
            problem("dispatcher takes no settings: dispatcher = {}".to_string());
            None
        }
        ("workflow", toml::Value::Table(table)) => {
            check_nested_keys(
                table,
                WORKFLOW_ACTION_KEYS,
                "the workflow action",
                line,
                out,
            );
            let Some(workflow) = text(table, "workflow") else {
                out.push(Diagnostic {
                    line,
                    message: format!("trigger `{trigger}`: a workflow action needs a workflow"),
                });
                return None;
            };
            Some(Action::Workflow {
                workflow,
                task: text(table, "task"),
                repo: text(table, "repo"),
                base: text(table, "base"),
                merge_into: text(table, "merge_into"),
                inputs: strings_map(trigger, "inputs", table.get("inputs"), line, out),
            })
        }
        ("workflow", _) => {
            problem("workflow action must be a table".to_string());
            None
        }
        ("push", toml::Value::Table(table)) => {
            check_nested_keys(table, PUSH_KEYS, "the push action", line, out);
            let Some(queue) = text(table, "queue") else {
                out.push(Diagnostic {
                    line,
                    message: format!("trigger `{trigger}`: a push action needs a queue"),
                });
                return None;
            };
            Some(Action::Push {
                queue,
                fields: strings_map(trigger, "fields", table.get("fields"), line, out),
            })
        }
        ("push", _) => {
            problem("push action must be a table".to_string());
            None
        }
        (other, _) => {
            problem(format!(
                "action \"{other}\" is not workflow, push or dispatcher"
            ));
            None
        }
    }
}

/// The fields of one lane kind that pools, queues and hooks add, as raw values with their lines.
/// The lane table crate refuses these keys; the validators of this crate read them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LaneFields {
    pub line: usize,
    /// Each `pulls` entry.
    pub pulls: Vec<Located<toml::Value>>,
    /// Each `needs` entry of `[kind.binding] pools`.
    pub pools: Vec<Located<toml::Value>>,
    /// Each hook point of `[kind.binding.hooks]`, by name.
    pub hooks: Vec<(String, Located<toml::Value>)>,
}

fn raw_value(value: &toml_edit::Value) -> Result<toml::Value, String> {
    let wrapped = format!("v = {value}");
    toml::from_str::<toml::Table>(&wrapped)
        .map_err(|error| error.message().to_string())?
        .remove("v")
        .ok_or_else(|| "empty value".to_string())
}

/// The values an item holds: the elements of an array (or array of tables), else the item itself.
fn located_values(
    item: &toml_edit::Item,
    lines: &LineIndex,
    context: &str,
) -> Result<Vec<Located<toml::Value>>, ParseError> {
    let whole_line = item.span().map_or(1, |span| lines.line(span.start));
    let convert = |value: toml_edit::Value, line: usize| {
        raw_value(&value)
            .map(|value| Located { line, value })
            .map_err(|message| ParseError {
                line,
                message: format!("{context}: {message}"),
            })
    };
    match item.clone().into_value() {
        Ok(toml_edit::Value::Array(array)) => array
            .iter()
            .map(|value| {
                let line = value
                    .span()
                    .map_or(whole_line, |span| lines.line(span.start));
                convert(value.clone(), line)
            })
            .collect(),
        Ok(value) => Ok(vec![convert(value, whole_line)?]),
        Err(_) => Err(ParseError {
            line: whole_line,
            message: format!("{context} must be a value"),
        }),
    }
}

/// Reads `pulls`, `[kind.binding] pools` and `[kind.binding.hooks]` of every kind of a lane table
/// text, keyed by kind name. Only syntax is checked here; the entries are raw.
pub fn lane_fields(lanes_toml: &str) -> Result<BTreeMap<String, LaneFields>, ParseError> {
    let lines = LineIndex::new(lanes_toml);
    let document = toml_edit::ImDocument::parse(lanes_toml)
        .map_err(|error| lines.error(error.span(), error.message()))?;
    let mut kinds = BTreeMap::new();
    for kind in children(document.as_table(), "kind") {
        let Some(name) = kind.get("name").and_then(toml_edit::Item::as_str) else {
            continue;
        };
        let line = kind
            .get_key_value("name")
            .and_then(|(key, _)| key.span())
            .map_or(1, |span| lines.line(span.start));
        let mut fields = LaneFields {
            line,
            ..LaneFields::default()
        };
        if let Some(pulls) = kind.get("pulls") {
            fields.pulls = located_values(pulls, &lines, "pulls")?;
        }
        for binding in children(kind, "binding") {
            if let Some(pools) = binding.get("pools") {
                fields.pools = located_values(pools, &lines, "pools")?;
            }
            for hooks in children(binding, "hooks") {
                for (hook, item) in hooks.iter() {
                    let hook_line = hooks
                        .get_key_value(hook)
                        .and_then(|(key, _)| key.span())
                        .map_or(line, |span| lines.line(span.start));
                    let value = item.clone().into_value().map_err(|_| ParseError {
                        line: hook_line,
                        message: format!("hook {hook} must be a table"),
                    })?;
                    let value = raw_value(&value).map_err(|message| ParseError {
                        line: hook_line,
                        message: format!("hook {hook}: {message}"),
                    })?;
                    fields.hooks.push((
                        hook.to_string(),
                        Located {
                            line: hook_line,
                            value,
                        },
                    ));
                }
            }
        }
        kinds.insert(name.to_string(), fields);
    }
    Ok(kinds)
}
