//! The preview of a diff as text (D7 step 6): a header, a summary line, then the sections Create,
//! Change, Cancel, Kept, Split and Untouched. The app shows it in a read-only buffer before it asks
//! to apply.

use std::collections::{BTreeSet, HashMap};

use bongos_work_model::commands::default_unblock;
use bongos_work_model::derive::{self, State};
use bongos_work_model::rules::{EdgeKind, role_name};
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Todo, TodoState, UnblockPoint};

use crate::diff::{Diff, KeeperSource, Op, SplitWork, TaskRef, TodoOp};
use crate::grammar::{Parsed, kind_name, unblock_name};

/// The preview text of `diff`, the diff of `parsed` against `tree`. It ends with a newline.
pub fn preview(parsed: &Parsed, tree: &WorkTree, diff: &Diff) -> String {
    let number = parsed
        .header
        .number
        .as_ref()
        .map_or_else(|| "?".to_string(), |number| number.value.to_string());
    let goal = tree
        .task_by_id(&diff.root)
        .map_or(String::new(), |task| task.goal().to_string());
    let mut lines = vec![format!("Proposal {number} for \"{goal}\"")];
    if diff.is_empty() {
        lines.push(format!(
            "Nothing to change: the tree already matches proposal {number}"
        ));
        return finish(lines);
    }
    if let Some(refusal) = &diff.refusal {
        lines.push(format!("Refused, nothing will be applied: {refusal}"));
    }
    let names = Names { tree };
    lines.push(summary(diff));
    lines.push(String::new());

    lines.push("Create".to_string());
    lines.extend(create_lines(diff));
    lines.push("Change".to_string());
    lines.extend(change_lines(diff, &names));
    lines.push("Cancel".to_string());
    for op in &diff.ops {
        if let Op::Cancel { id, with, .. } = op {
            let mut line = format!("  {}", names.slug(id));
            if !with.is_empty() {
                let with: Vec<String> = with.iter().map(|id| names.slug(id)).collect();
                line.push_str(&format!(" (with {})", with.join(", ")));
            }
            lines.push(line);
        }
    }
    lines.push("Kept".to_string());
    for op in &diff.ops {
        if let Op::Kept {
            id,
            line,
            what,
            reason,
        } = op
        {
            lines.push(format!(
                "  {}: kept, {what} (line {line}): {reason}",
                names.slug(id)
            ));
        }
    }
    lines.push("Split".to_string());
    lines.extend(split_lines(diff, &names));
    lines.push("Untouched".to_string());
    let untouched: Vec<String> = diff
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Untouched { id } => Some(names.with_state(id)),
            _ => None,
        })
        .collect();
    if !untouched.is_empty() {
        lines.push(format!("  {}", untouched.join(", ")));
    }
    finish(lines)
}

fn finish(lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

struct Names<'a> {
    tree: &'a WorkTree,
}

impl Names<'_> {
    fn slug(&self, id: &str) -> String {
        self.tree
            .task_by_id(id)
            .map_or_else(|| id.to_string(), |task| task.slug.clone())
    }

    /// The slug, with `(done)` or `(cancelled)` for a closed task.
    fn with_state(&self, id: &str) -> String {
        let Some(task) = self.tree.task_by_id(id) else {
            return id.to_string();
        };
        match derive::state(task, self.tree) {
            State::Done => format!("{} (done)", task.slug),
            State::Cancelled => format!("{} (cancelled)", task.slug),
            _ => task.slug.clone(),
        }
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// What a diff does, counted for the summary line.
struct Counts {
    tasks: usize,
    todos: usize,
    edges: usize,
    changed: usize,
    moved: usize,
    cancelled: usize,
    split: usize,
    kept: usize,
}

fn counts(diff: &Diff) -> Counts {
    let mut counts = Counts {
        tasks: 0,
        todos: 0,
        edges: 0,
        changed: 0,
        moved: 0,
        cancelled: 0,
        split: 0,
        kept: 0,
    };
    let mut changed = BTreeSet::new();
    for op in &diff.ops {
        match op {
            Op::Create { task, .. } => {
                counts.tasks += 1;
                counts.todos += task.todos.len();
            }
            Op::AddEdge { from, .. } => {
                counts.edges += 1;
                if let TaskRef::Existing { id, .. } = from {
                    changed.insert(id.as_str());
                }
            }
            Op::Rename { id, .. }
            | Op::SetKind { id, .. }
            | Op::SetPlannedResult { id, .. }
            | Op::Todos { id, .. }
            | Op::RemoveEdge { id, .. } => {
                changed.insert(id.as_str());
            }
            Op::Move { .. } => counts.moved += 1,
            Op::Cancel { .. } => counts.cancelled += 1,
            // A leaf that held only its plan todo becoming a group is the plan itself.
            Op::Split { keeper, work, .. } => {
                if keeper.is_some() || !only_plan(&work.todos) {
                    counts.split += 1;
                }
            }
            Op::Kept { .. } => counts.kept += 1,
            Op::PlanDone { .. } | Op::Untouched { .. } => {}
        }
    }
    counts.changed = changed.len();
    counts
}

/// The words of the summary line in the present (preview) or the past (applied).
struct Tense {
    create: &'static str,
    change: &'static str,
    moved: &'static str,
    cancel: &'static str,
    split: &'static str,
}

const PRESENT: Tense = Tense {
    create: "create",
    change: "change",
    moved: "move",
    cancel: "cancel",
    split: "split",
};

const PAST: Tense = Tense {
    create: "created",
    change: "changed",
    moved: "moved",
    cancel: "cancelled",
    split: "split",
};

/// `create 5 tasks, 14 todos, 6 edges` and what else changes, as `; change 2 tasks, move 1`.
fn counted(counts: &Counts, tense: &Tense) -> (String, Vec<String>) {
    let text = format!(
        "{} {}, {}, {}",
        tense.create,
        plural(counts.tasks, "task", "tasks"),
        plural(counts.todos, "todo", "todos"),
        plural(counts.edges, "edge", "edges")
    );
    let mut touched = Vec::new();
    if counts.changed > 0 {
        touched.push(format!(
            "{} {}",
            tense.change,
            plural(counts.changed, "task", "tasks")
        ));
    }
    if counts.moved > 0 {
        touched.push(format!("{} {}", tense.moved, counts.moved));
    }
    if counts.cancelled > 0 {
        touched.push(format!("{} {}", tense.cancel, counts.cancelled));
    }
    if counts.split > 0 {
        touched.push(format!("{} {}", tense.split, counts.split));
    }
    (text, touched)
}

/// `create 5 tasks, 14 todos, 6 edges; nothing existing is touched`, or with what changes.
pub fn summary(diff: &Diff) -> String {
    let counts = counts(diff);
    let (mut text, touched) = counted(&counts, &PRESENT);
    if touched.is_empty() {
        text.push_str("; nothing existing is touched");
    } else {
        text.push_str("; ");
        text.push_str(&touched.join(", "));
    }
    if counts.kept > 0 {
        text.push_str(&format!("; {} kept", counts.kept));
    }
    text
}

/// The line the toast shows and the root's history records once proposal `number` is applied:
/// `proposal 2 applied: created 5 tasks, 14 todos, 6 edges`, then what else changed and how many
/// asks were kept.
pub fn applied_summary(number: u32, diff: &Diff) -> String {
    let counts = counts(diff);
    let (mut text, touched) = counted(&counts, &PAST);
    if !touched.is_empty() {
        text.push_str("; ");
        text.push_str(&touched.join(", "));
    }
    if counts.kept > 0 {
        text.push_str(&format!("; {} kept", counts.kept));
    }
    format!("proposal {number} applied: {text}")
}

fn only_plan(todos: &[Todo]) -> bool {
    todos
        .iter()
        .all(|todo| todo.role == Some(bongos_work_model::TodoRole::Plans))
}

fn edge_phrase(kind: EdgeKind, target: &str, unblock: UnblockPoint) -> String {
    let word = match kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    };
    let mut text = format!("{word} {target}");
    if unblock != default_unblock(kind) {
        text.push_str(&format!(" | unblock {}", unblock_name(unblock)));
    }
    text
}

fn create_lines(diff: &Diff) -> Vec<String> {
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut groups = BTreeSet::new();
    let mut keepers = BTreeSet::new();
    for op in &diff.ops {
        match op {
            Op::Create {
                parent: TaskRef::New { line, .. },
                ..
            } => {
                groups.insert(*line);
            }
            Op::Split {
                keeper: Some(keeper),
                ..
            } => {
                keepers.insert(keeper.line);
            }
            _ => {}
        }
    }
    let mut lines = Vec::new();
    for op in &diff.ops {
        let Op::Create {
            line,
            parent,
            label,
            task,
        } = op
        else {
            continue;
        };
        let level = match parent {
            TaskRef::New { line, .. } => depth.get(line).copied().unwrap_or(0) + 1,
            TaskRef::Existing { .. } => 0,
        };
        depth.insert(*line, level);
        let mut details = vec![kind_name(task.kind).to_string()];
        if groups.contains(line) {
            details.push("group".to_string());
        } else {
            details.push(plural(task.todos.len(), "todo", "todos"));
        }
        if keepers.contains(line) {
            details.push("keeps work".to_string());
        }
        let edges: Vec<String> = diff
            .ops
            .iter()
            .filter_map(|edge| match edge {
                Op::AddEdge {
                    from: TaskRef::New { line: from, .. },
                    target,
                    kind,
                    unblock,
                    default,
                    ..
                } if from == line => {
                    let mut text = edge_phrase(*kind, target.name(), *unblock);
                    if *default {
                        text.push_str(" (default)");
                    }
                    Some(text)
                }
                _ => None,
            })
            .collect();
        let mut text = format!(
            "  {}{label}: {} ({}",
            "  ".repeat(level),
            task.goal,
            details.join(", ")
        );
        if !edges.is_empty() {
            text.push_str("; ");
            text.push_str(&edges.join(", "));
        }
        text.push(')');
        lines.push(text);
    }
    lines
}

fn todo_op_line(op: &TodoOp) -> String {
    match op {
        TodoOp::Add {
            name, after_frozen, ..
        } => match after_frozen {
            Some(frozen) => format!(
                "add todo \"{name}\" after \"{frozen}\", not before it: done and started todos keep their place"
            ),
            None => format!("add todo \"{name}\""),
        },
        TodoOp::Rename { index, from, to } => {
            format!("rename todo {} \"{from}\" -> \"{to}\"", index + 1)
        }
        TodoOp::SetRole { name, role } => {
            format!("todo \"{name}\" role {}", role.map_or("none", role_name))
        }
        TodoOp::SetWorkflow { name, workflow } => format!(
            "todo \"{name}\" workflow {}",
            workflow.as_deref().unwrap_or("none")
        ),
        TodoOp::Remove { index, name } => format!("remove todo {} \"{name}\"", index + 1),
        TodoOp::Move { name, from, to } => {
            format!("move todo \"{name}\" from {} to {}", from + 1, to + 1)
        }
    }
}

fn change_lines(diff: &Diff, names: &Names) -> Vec<String> {
    let mut lines = Vec::new();
    for op in &diff.ops {
        match op {
            Op::Rename { id, from, to, .. } => lines.push(format!(
                "  {}: rename \"{from}\" -> \"{to}\"",
                names.slug(id)
            )),
            Op::SetKind { id, from, kind, .. } => lines.push(format!(
                "  {}: kind {} -> {}",
                names.slug(id),
                kind_name(*from),
                kind_name(*kind)
            )),
            Op::SetPlannedResult { id, text, .. } => lines.push(match text {
                Some(text) => format!("  {}: result \"{text}\"", names.slug(id)),
                None => format!("  {}: result removed", names.slug(id)),
            }),
            Op::Todos { id, ops, .. } => {
                for todo in ops {
                    lines.push(format!("  {}: {}", names.slug(id), todo_op_line(todo)));
                }
            }
            Op::AddEdge {
                from: TaskRef::Existing { slug, .. },
                target,
                kind,
                unblock,
                ..
            } => lines.push(format!(
                "  {slug}: {}",
                edge_phrase(*kind, target.name(), *unblock)
            )),
            Op::RemoveEdge {
                id,
                target,
                kind,
                unblock,
                ..
            } => lines.push(format!(
                "  {}: remove {}",
                names.slug(id),
                edge_phrase(*kind, target, *unblock)
            )),
            Op::Move {
                id,
                new_parent,
                unlinked,
                ..
            } => {
                let mut line = format!("  {}: move under {}", names.slug(id), new_parent.name());
                if !unlinked.is_empty() {
                    let from: Vec<String> = unlinked.iter().map(|id| names.slug(id)).collect();
                    line.push_str(&format!(" (the edges of {} to it go)", from.join(", ")));
                }
                lines.push(line);
            }
            Op::PlanDone {
                id, todo, keeper, ..
            } => lines.push(match keeper {
                Some(keeper) => format!(
                    "  {}: todo \"{todo}\" done, in \"{}\"",
                    names.slug(id),
                    keeper.name()
                ),
                None => format!("  {}: todo \"{todo}\" done", names.slug(id)),
            }),
            _ => {}
        }
    }
    lines
}

fn todo_with_mark(todo: &Todo) -> String {
    let mark = match todo.state {
        TodoState::Done => Some("done"),
        TodoState::Skipped => Some("skipped"),
        TodoState::Cancelled => Some("cancelled"),
        TodoState::Open if todo.started.is_some() => Some("started"),
        TodoState::Open => None,
    };
    match mark {
        Some(mark) => format!("{} ({mark})", todo.name),
        None => todo.name.clone(),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn moved_work(work: &SplitWork) -> Vec<String> {
    let mut moved = Vec::new();
    if let Some(branch) = &work.branch {
        moved.push(format!("branch {branch}"));
    }
    if work.worktree.is_some() {
        moved.push("worktree".to_string());
    }
    if work.result {
        moved.push("result".to_string());
    }
    if work.planned_result {
        moved.push("planned result".to_string());
    }
    if work.chats > 0 {
        moved.push(plural(work.chats, "chat", "chats"));
    }
    if work.runs > 0 {
        moved.push(plural(work.runs, "run", "runs"));
    }
    if !work.todos.is_empty() {
        let todos: Vec<String> = work.todos.iter().map(todo_with_mark).collect();
        moved.push(format!("todos {}", todos.join(", ")));
    }
    moved
}

fn split_lines(diff: &Diff, names: &Names) -> Vec<String> {
    let mut lines = Vec::new();
    for op in &diff.ops {
        let Op::Split {
            id,
            keeper,
            work,
            todo_ops,
            ..
        } = op
        else {
            continue;
        };
        let slug = names.slug(id);
        match keeper {
            Some(keeper) => {
                let source = match keeper.source {
                    KeeperSource::Marked => "keeps work",
                    KeeperSource::Chosen => "keeps work, chosen",
                    KeeperSource::Default => "keeps work, default",
                };
                let moved = moved_work(work);
                let what = if moved.is_empty() {
                    "nothing".to_string()
                } else {
                    and_list(&moved)
                };
                let verb = if moved.len() == 1 { "moves" } else { "move" };
                lines.push(format!(
                    "  {slug} becomes a group; {what} {verb} into \"{}\" ({source})",
                    keeper.label
                ));
                for todo in todo_ops {
                    lines.push(format!("    {}: {}", keeper.label, todo_op_line(todo)));
                }
            }
            None if work.todos.is_empty() => lines.push(format!("  {slug} becomes a group")),
            None => {
                let todos: Vec<String> = work.todos.iter().map(todo_with_mark).collect();
                lines.push(format!(
                    "  {slug} becomes a group; its todos {} go to its history",
                    todos.join(", ")
                ));
            }
        }
    }
    lines
}
