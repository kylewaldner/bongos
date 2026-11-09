//! Proposals written later, and the automatic apply inside the writer's subtree (D13).
//!
//! A proposal file that appears or changes reaches the builder through the work store's watcher,
//! as a `Changed` event with a new mtime in `Task::files`. Each scan reads the new or changed
//! `proposals/<n>.proposal.txt` files on the background executor (the others come from the last
//! scan) and classifies every proposal against the new tree: applied or rejected as it is,
//! untouched (its diff is empty), with errors, waiting for Kyle, or one that may apply on its own.
//!
//! A proposal may apply on its own when `task_builder.enabled` is true, the effective switch of
//! its root's project is on (D15), its header says `by chat <id>` and that chat is indexed under a
//! task T, its root is T or under T, the root has no open `plans` todo, no record of its number
//! exists, it has no error, and its diff changes something without a drop, a move, a kept line, a
//! rename or kind change of the root, or a split whose keeper is not marked. Everything else
//! waits, and the "needs you" source (see [`crate::needs_you`]) reports it on its root.
//!
//! The auto-apply goes through the same writer command as `task: apply proposal`
//! ([`apply_command`]), by `agent`, and the decision is made again inside the writer, on the tree
//! it holds, the file read and the running work collected just before the write is queued:
//! never on the scanned snapshot. One proposal is applied at a time, in number order per root.
//!
//! Each file content (by hash) is judged for an auto-apply once: when it first shows up after the
//! first scan. So the builder's own writes (`task.toml`, which the watcher reports too) never
//! trigger an apply, a failed auto-apply is not retried until the file changes, and a proposal
//! that waited never applies on its own later. Files present at the first scan (at start, or when
//! the builder or the store is turned on) are only classified, never applied on their own.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use bongos_builder_model::apply::{APPLIED_EVENT, Request, file_hash};
use bongos_builder_model::{
    By, Choices, Diff, KeeperSource, LiveSet, Op, Parsed, Severity, applied_summary, diff,
    fingerprint,
};
use bongos_chat::ChatStore;
use bongos_work::model::commands::{Change, Context as WorkContext, Outcome, Write};
use bongos_work::model::store::{Task, WorkTree};
use bongos_work::model::{TodoRole, TodoState, now};
use bongos_work::{NeedsYouRegistry, WorkStore, WorkStoreEvent};
use gpui::{App, AppContext as _, Global};
use settings::{Settings as _, SettingsStore};
use workspace::{MultiWorkspace, Workspace};

use crate::actions::show_toast;
use crate::apply::apply_command;
use crate::auto_apply::switch_on;
use crate::choose::proposal_number;
use crate::{TaskBuilderSettings, live, view};

/// Who runs an auto-apply, in the history and the writer's context.
pub(crate) const AGENT: &str = "agent";

/// The chat a proposal names in its `by chat <id>` line: the task it is indexed under and its
/// name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatOwner {
    pub task_id: String,
    pub name: String,
}

/// Every chat indexed under a task, by id.
pub(crate) type Chats = HashMap<String, ChatOwner>;

/// The chats the tests say exist, instead of the chat store's index.
#[cfg(test)]
pub(crate) struct TestChats(pub Chats);

#[cfg(test)]
impl Global for TestChats {}

/// When set, every auto-apply fails inside the writer with this reason (tests only).
#[cfg(test)]
pub(crate) struct FailAutoApply(pub Option<String>);

#[cfg(test)]
impl Global for FailAutoApply {}

/// The settings an auto-apply reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Switch {
    /// `task_builder.enabled`.
    pub enabled: bool,
    /// `task_builder.auto_apply`, which a project's `auto_apply_proposals` overrides.
    pub global: bool,
    /// `task_builder.todo_warning`.
    pub todo_warning: u32,
}

impl Switch {
    fn read(cx: &App) -> Self {
        let settings = TaskBuilderSettings::get_global(cx);
        Self {
            enabled: settings.enabled,
            global: settings.auto_apply,
            todo_warning: settings.todo_warning,
        }
    }
}

/// What a proposal file is waiting for, as the last scan or auto-apply saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Applied or rejected as it is now.
    Settled,
    /// Its diff is empty, as an untouched file of `task: propose` is.
    Untouched,
    /// It has this many errors.
    Problems(usize),
    /// It waits for Kyle.
    Pending,
    /// Its auto-apply is queued or running.
    Applying,
    /// Its auto-apply failed for this reason; it waits for Kyle.
    Failed(String),
}

/// One proposal file of the last scan.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// The task whose directory holds the file, which is the proposal's root.
    pub task_id: String,
    pub number: u32,
    pub mtime: SystemTime,
    pub bytes: Arc<[u8]>,
    pub hash: String,
    pub status: Status,
    /// The scan found it may apply on its own.
    auto: bool,
}

/// The builder's view of the proposal files of the tree.
#[derive(Default)]
pub(crate) struct Later {
    /// By absolute path.
    pub entries: BTreeMap<PathBuf, Entry>,
    /// The hash of each file when it was last judged for an auto-apply.
    judged: HashMap<PathBuf, String>,
    /// The first scan since start (or since the builder or the store was turned on) is done.
    baseline_done: bool,
    scanning: bool,
    /// A `Changed` event came during the scan.
    dirty: bool,
    /// The proposals waiting for their auto-apply, in order.
    queue: VecDeque<PathBuf>,
    applying: bool,
    /// The `task_builder.enabled` the last settings change saw.
    enabled: bool,
    /// Auto-applies queued on the writer, for tests.
    pub attempts: usize,
}

impl Global for Later {}

impl Later {
    /// No scan or auto-apply is running or waiting.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        !self.scanning && !self.dirty && !self.applying && self.queue.is_empty()
    }
}

/// Starts watching: the store's `Changed` events and the builder's `enabled` setting.
pub(crate) fn install(cx: &mut App) {
    let enabled = TaskBuilderSettings::get_global(cx).enabled;
    cx.set_global(Later {
        enabled,
        ..Later::default()
    });
    if let Some(store) = WorkStore::global(cx) {
        cx.subscribe(&store, |_, event, cx| {
            if *event == WorkStoreEvent::Changed {
                changed(cx);
            }
        })
        .detach();
    }
    cx.observe_global::<SettingsStore>(|cx| {
        let enabled = TaskBuilderSettings::get_global(cx).enabled;
        if cx.global::<Later>().enabled != enabled {
            cx.global_mut::<Later>().enabled = enabled;
            changed(cx);
        }
    })
    .detach();
    changed(cx);
}

/// Something may have changed: scan, or scan again after the running scan.
fn changed(cx: &mut App) {
    let later = cx.global_mut::<Later>();
    if later.scanning {
        later.dirty = true;
        return;
    }
    scan(cx);
}

/// The chats indexed under a task, by id.
fn chats(cx: &App) -> Chats {
    #[cfg(test)]
    if let Some(chats) = cx.try_global::<TestChats>() {
        return chats.0.clone();
    }
    let Some(store) = ChatStore::global(cx) else {
        return Chats::default();
    };
    store
        .read(cx)
        .index()
        .values()
        .filter_map(|indexed| {
            let record = indexed.entry.record()?;
            Some((
                record.id.clone(),
                ChatOwner {
                    task_id: indexed.task_id.clone()?,
                    name: record.name.clone(),
                },
            ))
        })
        .collect()
}

/// Forgets every file, so the next scan is a first one again.
fn reset(later: &mut Later) {
    later.entries.clear();
    later.judged.clear();
    later.queue.clear();
    later.baseline_done = false;
}

fn scan(cx: &mut App) {
    let switch = Switch::read(cx);
    let tree = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
    let (Some(tree), true) = (tree, switch.enabled) else {
        // The store or the builder is off: nothing is classified.
        let later = cx.global_mut::<Later>();
        let had = !later.entries.is_empty();
        reset(later);
        if had {
            NeedsYouRegistry::changed(cx);
        }
        return;
    };
    let Some(store) = WorkStore::global(cx) else {
        return;
    };
    let fs = store.read(cx).fs();
    let live = live::collect(&tree, cx);
    let chats = chats(cx);
    let later = cx.global_mut::<Later>();
    later.scanning = true;
    later.dirty = false;
    let previous: HashMap<PathBuf, (SystemTime, Arc<[u8]>)> = later
        .entries
        .iter()
        .map(|(path, entry)| (path.clone(), (entry.mtime, entry.bytes.clone())))
        .collect();
    let read = cx.background_spawn(async move {
        let mut scanned = Vec::new();
        for task in tree.tasks() {
            let Some(task_id) = task.id() else {
                continue;
            };
            for (relative, mtime) in &task.files {
                let Some(number) = proposal_number(relative) else {
                    continue;
                };
                let path = task.dir.join(relative);
                let bytes = match previous.get(&path) {
                    Some((seen, bytes)) if seen == mtime => bytes.clone(),
                    _ => match fs.load_bytes(&path).await {
                        Ok(bytes) => Arc::from(bytes),
                        // Gone or unreadable since the store listed it: the next event says.
                        Err(_) => continue,
                    },
                };
                let hash = file_hash(&bytes);
                let (status, auto) =
                    classify(&bytes, number, task, &hash, &tree, &live, &chats, switch);
                scanned.push((
                    path,
                    Entry {
                        task_id: task_id.to_string(),
                        number,
                        mtime: *mtime,
                        bytes,
                        hash,
                        status,
                        auto,
                    },
                ));
            }
        }
        scanned
    });
    cx.spawn(async move |cx| {
        let scanned = read.await;
        cx.update(|cx| finish_scan(scanned, cx));
    })
    .detach();
}

/// The status of proposal `number` of `task` with the file `bytes` (of hash `hash`) on `tree`,
/// and whether it may apply on its own.
#[allow(clippy::too_many_arguments)]
fn classify(
    bytes: &[u8],
    number: u32,
    task: &Task,
    hash: &str,
    tree: &WorkTree,
    live: &LiveSet,
    chats: &Chats,
    switch: Switch,
) -> (Status, bool) {
    let settled = task.file.as_ref().is_some_and(|file| {
        file.proposals
            .iter()
            .rev()
            .find(|record| record.number == number)
            .is_some_and(|record| record.hash == hash)
    });
    if settled {
        return (Status::Settled, false);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (Status::Problems(1), false);
    };
    let Some(task_id) = task.id() else {
        return (Status::Pending, false);
    };
    let analysis = view::analyze(text, number, task_id, tree, live, switch.todo_warning);
    let errors = errors(&analysis.problems);
    if errors > 0 {
        return (Status::Problems(errors), false);
    }
    let diff = diff(&analysis.parsed, tree, live, &Choices::default());
    if diff.is_empty() {
        return (Status::Untouched, false);
    }
    let auto = wait_reason(&analysis.parsed, &diff, number, tree, chats, switch).is_none();
    (Status::Pending, auto)
}

fn errors(problems: &[bongos_builder_model::Problem]) -> usize {
    problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .count()
}

/// Why the proposal `parsed` (number `number`, with the diff `diff` against `tree`) waits for
/// Kyle, or `None` when it may apply on its own (D13). The proposal has no error.
pub(crate) fn wait_reason(
    parsed: &Parsed,
    diff: &Diff,
    number: u32,
    tree: &WorkTree,
    chats: &Chats,
    switch: Switch,
) -> Option<String> {
    if !switch.enabled {
        return Some("the task builder is off".to_string());
    }
    let chat_id = match parsed.header.by.as_ref().map(|by| &by.value) {
        Some(By::Chat(id)) => id,
        Some(By::Kyle) | None => return Some("written by Kyle".to_string()),
        Some(By::Workflow(name)) => return Some(format!("written by the workflow {name}")),
    };
    let Some(owner) = chats.get(chat_id) else {
        return Some(format!("chat {chat_id} is not on a task"));
    };
    let Some(root) = tree.task_by_id(&diff.root) else {
        return Some("its root is not in the tree".to_string());
    };
    let inside = tree
        .task_by_id(&owner.task_id)
        .is_some_and(|owner| root.dir.starts_with(&owner.dir));
    if !inside {
        return Some(format!(
            "{} is outside the subtree of chat {}",
            root.slug, owner.name
        ));
    }
    if !switch_on(tree, &diff.root, switch.global) {
        return Some("auto apply is off".to_string());
    }
    let Some(file) = root.file.as_ref() else {
        return Some(format!("the task file of {} does not read", root.slug));
    };
    let planning = file
        .todos
        .iter()
        .any(|todo| todo.role == Some(TodoRole::Plans) && todo.state == TodoState::Open);
    if planning {
        return Some("it plans its root".to_string());
    }
    if file.proposals.iter().any(|record| record.number == number) {
        return Some(format!("proposal {number} was applied or rejected before"));
    }
    if let Some(refusal) = &diff.refusal {
        return Some(refusal.clone());
    }
    if diff.is_empty() {
        return Some("it changes nothing".to_string());
    }
    diff.ops.iter().find_map(|op| match op {
        Op::Cancel { .. } => Some("it drops a task".to_string()),
        Op::Move { .. } => Some("it moves a task".to_string()),
        Op::Kept { .. } => Some("it has a kept line".to_string()),
        Op::Rename { id, .. } if *id == diff.root => Some("it renames its root".to_string()),
        Op::SetKind { id, .. } if *id == diff.root => {
            Some("it changes its root's kind".to_string())
        }
        Op::Split {
            keeper: Some(keeper),
            ..
        } if keeper.source != KeeperSource::Marked => {
            Some("it splits a leaf without marking the keeper".to_string())
        }
        _ => None,
    })
}

/// The history line and toast of an auto-apply: `agent applied proposal 2: added 1 task (chat
/// <name>): <why>`.
pub(crate) fn agent_line(number: u32, summary: &str, chat: &str, why: Option<&str>) -> String {
    let prefix = format!("proposal {number} applied: ");
    let what = summary.strip_prefix(&prefix).unwrap_or(summary);
    let mut line = format!("{AGENT} applied proposal {number}: {what} (chat {chat})");
    if let Some(why) = why.map(str::trim).filter(|why| !why.is_empty()) {
        line.push_str(": ");
        line.push_str(why);
    }
    line
}

/// The writer's command of an auto-apply of proposal `number` held by the task `holder`: the
/// decision of [`wait_reason`] taken again on the writer's `tree`, with the file's `bytes` and the
/// running work `live` collected just before the write was queued, then [`apply_command`], the
/// command of `task: apply proposal`, on the diff it computes here. The root's history says
/// [`agent_line`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn auto_apply_command(
    bytes: &[u8],
    holder: &str,
    number: u32,
    chats: &Chats,
    switch: Switch,
    tree: &WorkTree,
    ctx: &WorkContext,
    live: &LiveSet,
) -> Outcome {
    let text =
        std::str::from_utf8(bytes).map_err(|_| format!("proposal {number} is not UTF-8 text"))?;
    let analysis = view::analyze(text, number, holder, tree, live, switch.todo_warning);
    let errors = errors(&analysis.problems);
    if errors > 0 {
        return Err(format!("proposal {number} has errors"));
    }
    let parsed = analysis.parsed;
    let choices = Choices::default();
    let diff = diff(&parsed, tree, live, &choices);
    if let Some(reason) = wait_reason(&parsed, &diff, number, tree, chats, switch) {
        return Err(format!("not applied on its own: {reason}"));
    }
    let chat = match parsed.header.by.as_ref().map(|by| &by.value) {
        Some(By::Chat(id)) => chats.get(id).map_or(id.as_str(), |owner| &owner.name),
        _ => "",
    };
    let summary = applied_summary(number, &diff);
    let line = agent_line(
        number,
        &summary,
        chat,
        parsed.header.why.as_ref().map(|why| why.value.as_str()),
    );
    let request = Request {
        fingerprint: fingerprint(&diff, &choices, &file_hash(bytes)),
        parsed,
        choices,
    };
    let mut change = apply_command(&request, holder, number, tree, ctx, live, bytes)?;
    relabel(&mut change, holder, &summary, &line);
    Ok(change)
}

/// Puts `line` in place of `summary` at the start of the root's `proposal applied` entry.
fn relabel(change: &mut Change, root: &str, summary: &str, line: &str) {
    let swap = |detail: &mut Option<String>| {
        if let Some(text) = detail
            && let Some(rest) = text.strip_prefix(summary)
        {
            *text = format!("{line}{rest}");
        }
    };
    swap(&mut change.entry.detail);
    for write in &mut change.writes {
        if let Write::Task { file, .. } = write
            && file.id == root
            && let Some(entry) = file.history.last_mut()
            && entry.event == APPLIED_EVENT
        {
            swap(&mut entry.detail);
        }
    }
}

/// Takes the scan in: keeps the state of auto-applies running or failed on unchanged files,
/// judges every new file content once, queues the ones that may apply on their own and tells
/// the registry.
fn finish_scan(scanned: Vec<(PathBuf, Entry)>, cx: &mut App) {
    let later = cx.global_mut::<Later>();
    later.scanning = false;
    let baseline = !later.baseline_done;
    later.baseline_done = true;
    let mut entries = BTreeMap::new();
    let mut queued = Vec::new();
    for (path, mut entry) in scanned {
        if let Some(old) = later.entries.get(&path)
            && old.hash == entry.hash
            && entry.status == Status::Pending
            && matches!(old.status, Status::Applying | Status::Failed(_))
        {
            entry.status = old.status.clone();
        }
        if later.judged.get(&path) != Some(&entry.hash) {
            later.judged.insert(path.clone(), entry.hash.clone());
            if !baseline && entry.auto {
                entry.status = Status::Applying;
                queued.push((entry.task_id.clone(), entry.number, path.clone()));
            }
        }
        entries.insert(path, entry);
    }
    later.judged.retain(|path, _| entries.contains_key(path));
    later.queue.retain(|path| entries.contains_key(path));
    later.entries = entries;
    queued.sort();
    later
        .queue
        .extend(queued.into_iter().map(|(_, _, path)| path));
    let again = later.dirty;
    NeedsYouRegistry::changed(cx);
    if again {
        changed(cx);
    }
    run_queue(cx);
}

/// Starts the next queued auto-apply unless one is running.
fn run_queue(cx: &mut App) {
    let later = cx.global_mut::<Later>();
    if later.applying {
        return;
    }
    let Some((path, entry)) = std::iter::from_fn(|| later.queue.pop_front())
        .find_map(|path| Some((path.clone(), later.entries.get(&path)?.clone())))
    else {
        return;
    };
    let Some(store) = WorkStore::global(cx) else {
        return;
    };
    let later = cx.global_mut::<Later>();
    later.applying = true;
    later.attempts += 1;
    let fs = store.read(cx).fs();
    #[cfg(test)]
    let injected = cx
        .try_global::<FailAutoApply>()
        .and_then(|fail| fail.0.clone());
    cx.spawn(async move |cx| {
        let number = entry.number;
        // The file as it is now, never the scanned bytes.
        let result = match fs.load_bytes(&path).await {
            Err(error) => Err(format!("could not read proposal {number}: {error}")),
            Ok(bytes) => {
                let write = cx.update(|cx| {
                    let hash = file_hash(&bytes);
                    cx.global_mut::<Later>().judged.insert(path.clone(), hash);
                    // The running work, the chats and the settings now, right before the write
                    // is queued.
                    let live = store
                        .read(cx)
                        .snapshot()
                        .map(|now| live::collect(&now, cx))
                        .unwrap_or_default();
                    let chats = chats(cx);
                    let switch = Switch::read(cx);
                    let holder = entry.task_id.clone();
                    store.update(cx, |store, cx| {
                        store.write(
                            WorkContext::new(AGENT, now()),
                            move |tree, ctx| {
                                #[cfg(test)]
                                if let Some(reason) = &injected {
                                    return Err(reason.clone());
                                }
                                auto_apply_command(
                                    &bytes, &holder, number, &chats, switch, tree, ctx, &live,
                                )
                            },
                            cx,
                        )
                    })
                });
                write
                    .await
                    .map(|change| change.entry.detail.unwrap_or_default())
                    .map_err(|error| error.to_string())
            }
        };
        cx.update(|cx| finish_apply(path, result, cx));
    })
    .detach();
}

/// Records how the auto-apply went, toasts a success and starts the next one.
fn finish_apply(path: PathBuf, result: Result<String, String>, cx: &mut App) {
    let later = cx.global_mut::<Later>();
    later.applying = false;
    let status = match &result {
        Ok(_) => Status::Settled,
        Err(error) => Status::Failed(error.clone()),
    };
    if let Some(entry) = later.entries.get_mut(&path) {
        entry.status = status;
    }
    if let Ok(line) = result {
        toast(line, cx);
    }
    NeedsYouRegistry::changed(cx);
    run_queue(cx);
}

/// Shows `message` in the active window's workspace, else in any.
fn toast(message: String, cx: &mut App) {
    let active = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>());
    let others = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>());
    let workspace: Option<gpui::Entity<Workspace>> = active
        .into_iter()
        .chain(others)
        .find_map(|window| Some(window.read(cx).ok()?.workspace().clone()));
    match workspace {
        Some(workspace) => workspace.update(cx, |workspace, cx| show_toast(workspace, message, cx)),
        None => log::info!("{message}"),
    }
}
