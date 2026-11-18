//! The effective set of checks of a task: what it inherits, the named sets it uses, its own
//! entries, what it drops, and the closing checks of the groups above it, each row with where it
//! comes from. Pure over the parsed definitions.
//!
//! Document and spike tasks have no code tree, so no checks apply to them. A group is a task with
//! children (or the top group): its ordinary rows are passed down to its children and judged
//! there, and what it owes itself is its closing set.

use std::collections::BTreeMap;

use bongos_project_repo_model::{DefinitionNode, DefinitionTree};
use bongos_work_model::Kind;

use crate::decl::{self, CheckDecl, ChecksTable, Finding, Inherit};

mod loosening;

pub use loosening::{Loosening, LooseningKind, loosening};

const TASK_FILE: &str = "task.toml";

/// Where a declaration is written. The line is only there when the text was parsed with
/// [`decl::parse_file`]; the checks table of a task is read without spans and has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// Relative to `definitions/`.
    pub file: String,
    pub check: String,
    pub line: Option<usize>,
}

/// Why a row is in the set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Written in the task's own table.
    Own,
    /// Added by a named set the task uses.
    Set { set: String },
    /// Inherited; `from` is the task that declares it.
    Inherited { from: String },
    /// Replaces a row of the same name, which came `from` this source.
    Overrides { from: Box<Source> },
    /// A closing check of `group`, carried by this task as the last open child.
    Closing { group: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub decl: CheckDecl,
    pub declared: Declared,
    pub source: Source,
}

/// A row the task removed with `drop`, kept so the section can show it struck.
#[derive(Clone, Debug, PartialEq)]
pub struct Dropped {
    /// The task whose `drop` removed it.
    pub by: String,
    pub row: Row,
}

/// Whether checks apply to a task, and whether it has any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applicability {
    /// The task owes the rows it judges (see [`EffectiveSet::judged`]).
    Checks,
    /// Checks apply but there is nothing to judge: shown as "no checks", never as passing.
    NoChecks,
    /// A document or spike task: "no checks apply".
    NoChecksApply,
    /// The task is not in the definitions.
    UnknownTask,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSet {
    pub task_id: String,
    pub applicability: Applicability,
    /// Whether the task has children (or is the top group).
    pub is_group: bool,
    /// What the task's own `inherit` says.
    pub inherit: Inherit,
    /// The ordinary rows. On a task without children these are judged on it, including the
    /// closing checks it carries. On a group they pass down to its children.
    pub rows: Vec<Row>,
    /// A group's closing set: its own entries and the closing checks passed down to it.
    pub closing: Vec<Row>,
    pub dropped: Vec<Dropped>,
    /// Problems in the task's table, its sets and its rows.
    pub findings: Vec<Finding>,
}

impl EffectiveSet {
    fn empty(task_id: &str, applicability: Applicability) -> Self {
        Self {
            task_id: task_id.to_string(),
            applicability,
            is_group: false,
            inherit: Inherit::Parent,
            rows: Vec::new(),
            closing: Vec::new(),
            dropped: Vec::new(),
            findings: Vec::new(),
        }
    }

    /// The rows the task itself is judged on: its rows, or a group's closing set.
    pub fn judged(&self) -> &[Row] {
        match self.applicability {
            Applicability::NoChecksApply | Applicability::UnknownTask => &[],
            _ if self.is_group => &self.closing,
            _ => &self.rows,
        }
    }

    /// The rows judged on the task that are required.
    pub fn required(&self) -> impl Iterator<Item = &Row> {
        self.judged().iter().filter(|row| row.decl.is_required())
    }
}

/// One named set, `definitions/checks/sets/<name>.toml`.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedSet {
    pub name: String,
    pub file: String,
    pub check: Vec<CheckDecl>,
    pub closing: Vec<CheckDecl>,
}

/// The named sets, parsed from `(path, text)` pairs of `definitions/checks/sets/*.toml`. A set's
/// name is its file name without `.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamedSets {
    sets: BTreeMap<String, NamedSet>,
    /// Files that are not TOML, entries that could not be read, and two files of one name.
    pub findings: Vec<Finding>,
}

impl NamedSets {
    pub fn parse<P: AsRef<str>, T: AsRef<str>>(files: impl IntoIterator<Item = (P, T)>) -> Self {
        let mut named = Self::default();
        for (path, text) in files {
            let path = path.as_ref();
            let table = match decl::parse_file(path, text.as_ref()) {
                Ok(table) => table,
                Err(finding) => {
                    named.findings.push(finding);
                    continue;
                }
            };
            named.findings.extend(table.findings);
            let file_name = path.rsplit('/').next().unwrap_or(path);
            let name = file_name.strip_suffix(".toml").unwrap_or(file_name);
            let set = NamedSet {
                name: name.to_string(),
                file: path.to_string(),
                check: table.check,
                closing: table.closing,
            };
            if let Some(other) = named.sets.get(name) {
                named.findings.push(Finding::error(
                    path,
                    None,
                    None,
                    format!(
                        "set {name} is also defined by {}; this file is ignored",
                        other.file
                    ),
                ));
            } else {
                named.sets.insert(name.to_string(), set);
            }
        }
        named
    }

    pub fn get(&self, name: &str) -> Option<&NamedSet> {
        self.sets.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sets.keys().map(String::as_str)
    }
}

/// What a task owes at one level of the chain from the top group.
struct Level {
    rows: Vec<Row>,
    closing: Vec<Row>,
}

/// The effective set of `task_id`. `done` says whether a task is Done (callers answer true for a
/// cancelled task too): a group's closing checks go to the one child that is not done while every
/// other child is, never to a document or spike child, and to no child while two are open.
pub fn effective_set(
    task_id: &str,
    tree: &DefinitionTree,
    sets: &NamedSets,
    done: &dyn Fn(&str) -> bool,
) -> EffectiveSet {
    let mut chain = Vec::new();
    let found = tree
        .top
        .as_ref()
        .is_some_and(|top| find_chain(top, task_id, &mut chain));
    if !found {
        return EffectiveSet::empty(task_id, Applicability::UnknownTask);
    }
    let mut set = EffectiveSet::empty(task_id, Applicability::NoChecks);
    let mut above: Option<(&DefinitionNode, Level)> = None;
    for (depth, node) in chain.iter().copied().enumerate() {
        let is_target = depth + 1 == chain.len();
        let is_group = node.folder.is_empty() || !node.children.is_empty();
        let id = node.definition.id.as_str();
        let file = if node.folder.is_empty() {
            TASK_FILE.to_string()
        } else {
            format!("{}/{TASK_FILE}", node.folder)
        };
        let table = match node.definition.extra.get("checks") {
            Some(value) => decl::parse(&file, value),
            None => ChecksTable {
                file: file.clone(),
                ..ChecksTable::default()
            },
        };
        let mut findings = table.findings.clone();

        let mut rows: Vec<Row> = Vec::new();
        if table.inherit == Inherit::Parent
            && let Some((_, level)) = &above
        {
            rows.extend(
                level
                    .rows
                    .iter()
                    .map(|row| inherited(row, parent_id(&above))),
            );
        }
        let mut closing: Vec<Row> = Vec::new();
        for name in &table.uses {
            let Some(named) = sets.get(name) else {
                findings.push(Finding::error(
                    &file,
                    None,
                    None,
                    format!("use names the set {name}, which does not exist"),
                ));
                continue;
            };
            let source = Source::Set { set: name.clone() };
            for decl in &named.check {
                put(&mut rows, row_of(decl, &named.file, source.clone()), false);
            }
            for decl in &named.closing {
                put(
                    &mut closing,
                    row_of(decl, &named.file, source.clone()),
                    false,
                );
            }
        }
        for decl in &table.check {
            put(&mut rows, row_of(decl, &file, Source::Own), false);
        }
        for decl in &table.closing {
            put(&mut closing, row_of(decl, &file, Source::Own), false);
        }
        let mut dropped = Vec::new();
        for name in &table.drop {
            match rows.iter().position(|row| &row.decl.name == name) {
                Some(index) => dropped.push(Dropped {
                    by: id.to_string(),
                    row: rows.remove(index),
                }),
                None => findings.push(Finding::warning(
                    &file,
                    None,
                    Some(name),
                    "drop names a check that is not in the set",
                )),
            }
        }
        if let Some((group, level)) = &above {
            for row in carried(group, level, node, done) {
                put(if is_group { &mut closing } else { &mut rows }, row, true);
            }
        }
        if !is_group {
            if is_target && !table.closing.is_empty() {
                findings.push(Finding::warning(
                    &file,
                    table.closing[0].line,
                    Some(&table.closing[0].name),
                    "closing checks only apply to a group; ignored",
                ));
            }
            closing.clear();
        }

        if is_target {
            set.is_group = is_group;
            set.inherit = table.inherit;
            set.rows = rows;
            set.closing = closing;
            set.dropped = dropped;
            set.findings = findings;
            break;
        }
        above = Some((node, Level { rows, closing }));
    }
    set.findings.extend(or_accepted_findings(&set));
    set.applicability = match chain.last().map(|node| node.definition.kind) {
        Some(Kind::Document | Kind::Spike) => {
            set.rows.clear();
            set.closing.clear();
            Applicability::NoChecksApply
        }
        _ if set.judged().is_empty() => Applicability::NoChecks,
        _ => Applicability::Checks,
    };
    set
}

fn find_chain<'a>(node: &'a DefinitionNode, id: &str, chain: &mut Vec<&'a DefinitionNode>) -> bool {
    chain.push(node);
    if node.definition.id == id {
        return true;
    }
    if node
        .children
        .iter()
        .any(|child| find_chain(child, id, chain))
    {
        return true;
    }
    chain.pop();
    false
}

fn parent_id<'a>(above: &'a Option<(&DefinitionNode, Level)>) -> &'a str {
    above
        .as_ref()
        .map_or("", |(node, _)| node.definition.id.as_str())
}

fn row_of(decl: &CheckDecl, file: &str, source: Source) -> Row {
    Row {
        decl: decl.clone(),
        declared: Declared {
            file: file.to_string(),
            check: decl.name.clone(),
            line: decl.line,
        },
        source,
    }
}

/// A parent's row as its child sees it: inherited from the task that declares it.
fn inherited(row: &Row, parent: &str) -> Row {
    let from = match &row.source {
        Source::Inherited { from } => from.clone(),
        _ => parent.to_string(),
    };
    Row {
        source: Source::Inherited { from },
        ..row.clone()
    }
}

/// Adds `row`, or replaces the row of the same name. A replacement says so in its source unless
/// `keep_source` (the closing checks of a group, which keep saying whose they are).
fn put(rows: &mut Vec<Row>, mut row: Row, keep_source: bool) {
    match rows
        .iter_mut()
        .find(|other| other.decl.name == row.decl.name)
    {
        Some(slot) => {
            if !keep_source {
                row.source = Source::Overrides {
                    from: Box::new(slot.source.clone()),
                };
            }
            *slot = row;
        }
        None => rows.push(row),
    }
}

/// The closing checks `group` passes to `child`: all of them when the child is the one task left
/// open, none otherwise. A document or spike task never carries them.
fn carried(
    group: &DefinitionNode,
    level: &Level,
    child: &DefinitionNode,
    done: &dyn Fn(&str) -> bool,
) -> Vec<Row> {
    let id = child.definition.id.as_str();
    let carries = !level.closing.is_empty()
        && !matches!(child.definition.kind, Kind::Document | Kind::Spike)
        && !done(id)
        && group
            .children
            .iter()
            .all(|other| other.definition.id == id || done(&other.definition.id));
    if !carries {
        return Vec::new();
    }
    level
        .closing
        .iter()
        .map(|row| Row {
            source: match &row.source {
                Source::Closing { group } => Source::Closing {
                    group: group.clone(),
                },
                _ => Source::Closing {
                    group: group.definition.id.clone(),
                },
            },
            ..row.clone()
        })
        .collect()
}

/// `or_accepted` must name another check of the same effective set.
fn or_accepted_findings(set: &EffectiveSet) -> Vec<Finding> {
    let mut findings = Vec::new();
    for row in set.rows.iter().chain(&set.closing) {
        let Some(accepted) = &row.decl.or_accepted else {
            continue;
        };
        let message = if *accepted == row.decl.name {
            "or_accepted names the check itself".to_string()
        } else if !set
            .rows
            .iter()
            .chain(&set.closing)
            .any(|other| other.decl.name == *accepted)
        {
            format!("or_accepted names {accepted}, which is not in the effective set")
        } else {
            continue;
        };
        findings.push(Finding::error(
            &row.declared.file,
            row.declared.line,
            Some(&row.declared.check),
            message,
        ));
    }
    findings
}
