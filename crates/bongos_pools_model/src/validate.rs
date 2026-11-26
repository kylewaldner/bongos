//! Per-folder validation: every problem names its file and line. Blank text (a missing file)
//! has no problems.

use std::collections::BTreeMap;

use crate::decl::{
    Action, FieldType, GuardCheck, PoolDecl, PoolKind, PoolScope, QueueDecl, TriggerDecl,
    parse_pools, parse_queues, parse_triggers,
};
use crate::schedule::parse_duration;
use crate::source::Diagnostic;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} line {}: {}", self.file, self.line, self.message)
    }
}

fn problem(file: &str, line: usize, message: impl Into<String>) -> Problem {
    Problem {
        file: file.to_string(),
        line,
        message: message.into(),
    }
}

fn finish(file: &str, diagnostics: Vec<Diagnostic>, mut extra: Vec<Problem>) -> Vec<Problem> {
    extra.extend(
        diagnostics
            .into_iter()
            .map(|diagnostic| problem(file, diagnostic.line, diagnostic.message)),
    );
    extra.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.message.cmp(&b.message)));
    extra
}

/// A name is `[a-z][a-z0-9-]*`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A definition path: inside `definitions/`, so no `..` and not absolute.
pub fn definition_path_problem(path: &str) -> Option<&'static str> {
    if path.trim().is_empty() {
        Some("is empty")
    } else if path.starts_with('/') || path.contains(":\\") {
        Some("must be a definition path, not an absolute path")
    } else if path.split(['/', '\\']).any(|part| part == "..") {
        Some("must stay inside definitions/ (no `..`)")
    } else {
        None
    }
}

fn check_names<'a>(
    file: &str,
    what: &str,
    names: impl Iterator<Item = (&'a str, usize)>,
    out: &mut Vec<Problem>,
) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (name, line) in names {
        if !is_valid_name(name) {
            out.push(problem(
                file,
                line,
                format!("{what} name \"{name}\" must match [a-z][a-z0-9-]*"),
            ));
        }
        if let Some(first) = seen.insert(name, line) {
            out.push(problem(
                file,
                line,
                format!("{what} `{name}` is already declared on line {first}"),
            ));
        }
    }
}

pub fn validate_pools(file: &str, text: &str) -> Vec<Problem> {
    let parsed = match parse_pools(text) {
        Ok(parsed) => parsed,
        Err(error) => return vec![problem(file, error.line, error.message)],
    };
    let mut out = Vec::new();
    check_names(
        file,
        "pool",
        parsed
            .decls
            .iter()
            .map(|pool| (pool.name.as_str(), pool.line)),
        &mut out,
    );
    for pool in &parsed.decls {
        pool_rules(file, pool, &mut out);
    }
    finish(file, parsed.diagnostics, out)
}

fn pool_rules(file: &str, pool: &PoolDecl, out: &mut Vec<Problem>) {
    let name = &pool.name;
    match (pool.kind, pool.size) {
        (PoolKind::Slots, None) => out.push(problem(
            file,
            pool.line,
            format!("slots pool `{name}` needs a size of at least 1"),
        )),
        (PoolKind::Slots, Some(0)) => out.push(problem(
            file,
            pool.key_line("size"),
            format!("pool `{name}`: size must be at least 1"),
        )),
        (PoolKind::Slots, Some(_)) | (_, None) => {}
        (kind, Some(_)) => out.push(problem(
            file,
            pool.key_line("size"),
            format!("pool `{name}`: a {} pool has no size", kind.word()),
        )),
    }
    match (pool.kind, &pool.guard) {
        (PoolKind::Guard, None) => out.push(problem(
            file,
            pool.line,
            format!("guard pool `{name}` needs a guard"),
        )),
        (PoolKind::Rw | PoolKind::Mutex, Some(guard)) => out.push(problem(
            file,
            guard.line,
            format!("pool `{name}`: a {} pool takes no guard", pool.kind.word()),
        )),
        _ => {}
    }
    if let Some(guard) = &pool.guard
        && pool.scope == PoolScope::Machine
    {
        match &guard.check {
            GuardCheck::Script(_) => out.push(problem(
                file,
                guard.line,
                format!("machine pool `{name}` cannot have a script guard, because a script belongs to one project"),
            )),
            GuardCheck::FreeDiskGb {
                path: Some(path), ..
            } if path.contains("{project_folder}") => out.push(problem(
                file,
                guard.line,
                format!("machine pool `{name}` needs a literal guard path, because {{project_folder}} belongs to one project"),
            )),
            _ => {}
        }
    }
    if let Some(guard) = &pool.guard
        && let GuardCheck::Script(script) = &guard.check
        && let Some(reason) = definition_path_problem(script)
    {
        out.push(problem(
            file,
            guard.line,
            format!("pool `{name}`: guard script \"{script}\" {reason}"),
        ));
    }
    if let Some(max_hold) = &pool.max_hold
        && let Err(message) = parse_duration(max_hold)
    {
        out.push(problem(
            file,
            pool.key_line("max_hold"),
            format!("pool `{name}`: max_hold {message}"),
        ));
    }
}

pub fn validate_queues(file: &str, text: &str) -> Vec<Problem> {
    let parsed = match parse_queues(text) {
        Ok(parsed) => parsed,
        Err(error) => return vec![problem(file, error.line, error.message)],
    };
    let mut out = Vec::new();
    check_names(
        file,
        "queue",
        parsed
            .decls
            .iter()
            .map(|queue| (queue.name.as_str(), queue.line)),
        &mut out,
    );
    for queue in &parsed.decls {
        queue_rules(file, queue, &mut out);
    }
    finish(file, parsed.diagnostics, out)
}

fn queue_rules(file: &str, queue: &QueueDecl, out: &mut Vec<Problem>) {
    if let Some(task_field) = &queue.task_field {
        let line = queue.key_line("task_field");
        match queue.item.iter().find(|field| &field.name == task_field) {
            None => out.push(problem(
                file,
                line,
                format!(
                    "queue `{}`: task_field `{task_field}` is not an item field",
                    queue.name
                ),
            )),
            Some(field) if field.ty != FieldType::Task => out.push(problem(
                file,
                line,
                format!(
                    "queue `{}`: task_field `{task_field}` is not of type task",
                    queue.name
                ),
            )),
            Some(_) => {}
        }
    }
    if queue.max_attempts == Some(0) {
        out.push(problem(
            file,
            queue.key_line("max_attempts"),
            format!("queue `{}`: max_attempts must be at least 1", queue.name),
        ));
    }
}

pub fn validate_triggers(file: &str, text: &str) -> Vec<Problem> {
    let parsed = match parse_triggers(text) {
        Ok(parsed) => parsed,
        Err(error) => return vec![problem(file, error.line, error.message)],
    };
    let mut out = Vec::new();
    check_names(
        file,
        "trigger",
        parsed
            .decls
            .iter()
            .map(|trigger| (trigger.name.as_str(), trigger.line)),
        &mut out,
    );
    for trigger in &parsed.decls {
        trigger_rules(file, trigger, &mut out);
    }
    finish(file, parsed.diagnostics, out)
}

fn trigger_rules(file: &str, trigger: &TriggerDecl, out: &mut Vec<Problem>) {
    if let Action::Workflow {
        repo: None,
        base,
        merge_into,
        ..
    } = &trigger.action
        && (base.is_some() || merge_into.is_some())
    {
        out.push(problem(
            file,
            trigger.key_line("action"),
            format!("trigger `{}`: base and merge_into need repo", trigger.name),
        ));
    }
}
