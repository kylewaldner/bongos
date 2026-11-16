//! Which finished tasks owe checks: the `checks-owed` state apply hook. It runs at the end of
//! every writer transaction that writes task rows or moves the verified commit, before the
//! commit, and keeps one marker row per finished task that owes checks, `checks.owed/<task id>`,
//! written in the same transaction as the write that finished the task. No timestamps and no
//! sweep: a task is held while it has a marker and no valid done record, so a crash right after
//! the finishing write leaves it held, and nothing is recomputed at start.
//!
//! "Done-looking" is the Done the Work view would derive for the task if no hold existed: a leaf
//! whose todos are all closed or skipped, or whose status is Done. For each task a transaction
//! touches:
//! - a leaf that becomes Done-looking while `task_checks.enabled` is on in this process, whose
//!   kind takes checks and whose ordinary required set (from the definitions in force after the
//!   transaction) is not empty, gets its marker;
//! - a task that is not Done-looking afterward loses its marker and its done record, whether the
//!   setting is on or off, so a task reopened and finished again while it is off is not held
//!   later by an old marker;
//! - any other task keeps or lacks its marker as it was: a check added to a task that is already
//!   finished never marks it, and turning the setting off and on changes nothing.
//!
//! Groups (their closing checks, the last child to finish) are not judged here yet: a task with
//! children is never marked (a leaf that gains children loses its marker and done record), and a
//! leaf's marker names no group. Before the project's definitions are loaded nothing is marked,
//! and only a reopen the state record alone shows deletes.
//!
//! The hook writes only `kv` rows, runs no git and no process, and any error aborts the whole
//! transaction, so no task is finished without its marker. Its one read outside the transaction
//! is the named check sets of the project's master checkout, which the definitions tree does not
//! carry; when they cannot be read and the task uses one, the task is marked, and the decider
//! judges it with the sets it reads.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};
use bongos_checks_model::{Applicability, NamedSets, Source, decl, effective_set};
use bongos_project_repo::{StateApplyChange, StateApplyHook, StateApplyHooks, StateTx};
use bongos_project_repo_model::{
    DefinitionNode, DefinitionTree, TaskDefinition, TaskStateRecord, join,
};
use bongos_work::OpenProjectRepos;
use bongos_work::model::derive::{self, State};
use bongos_work::model::store::{Task, WorkTree};
use bongos_work::model::{ResultState, Status, TodoState};
use gpui::App;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};

use crate::definitions::named_sets;
use crate::evidence_store::{DONE_NAMESPACE, OWED_NAMESPACE};
use crate::pin::master_definitions;
use crate::settings::TaskChecksSettings;

/// The name of the hook among the state apply hooks.
pub const CHECKS_OWED_HOOK: &str = "checks-owed";

/// The marker of a finished task that owes checks, `checks.owed/<task id>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwedMarker {
    /// The groups whose closing checks this task carries as their last open child. Always
    /// empty for now: closing checks are not carried yet.
    #[serde(default)]
    pub closing_from: Vec<String>,
    /// On a group's marker only: the last child to become Done-looking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_child: Option<LastChild>,
}

/// The last child of a group to become Done-looking, with the generation that orders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastChild {
    pub id: String,
    pub generation: u64,
}

/// Writes the marker of `task_id`.
pub fn put_marker(tx: &mut StateTx, task_id: &str, marker: &OwedMarker) -> Result<()> {
    let value = toml::Value::try_from(marker).context("serializing an owed marker")?;
    tx.kv(OWED_NAMESPACE).put(task_id, value)
}

/// The marker of `task_id` as `tx` sees it.
pub fn marker_in(tx: &mut StateTx, task_id: &str) -> Result<Option<OwedMarker>> {
    tx.kv(OWED_NAMESPACE)
        .get(task_id)?
        .map(|value| {
            value
                .try_into()
                .with_context(|| format!("reading the owed marker of {task_id}"))
        })
        .transpose()
}

/// The named check sets of a project, by the project id in its `project.toml`. `None` when the
/// project is not known here.
pub trait NamedSetsLookup: Send + Sync + 'static {
    fn named_sets(&self, project_id: &str) -> Option<NamedSets>;
}

/// The named sets of the open project repositories, read from their master checkouts. Those may
/// differ from the verified definitions the hook judges by (an unlanded change to a set); a set
/// that cannot be read there counts as unknown, and a task that uses one is marked, so a stale
/// or missing checkout never lets a task finish unchecked.
struct OpenReposSets(OpenProjectRepos);

impl NamedSetsLookup for OpenReposSets {
    fn named_sets(&self, project_id: &str) -> Option<NamedSets> {
        let loaded = self
            .0
            .list()
            .into_iter()
            .find(|loaded| loaded.repo().location().id == project_id)?;
        Some(named_sets(&master_definitions(loaded.repo())))
    }
}

/// Named sets read from one `definitions/` folder, whatever the project.
pub struct FolderSets(pub PathBuf);

impl NamedSetsLookup for FolderSets {
    fn named_sets(&self, _project_id: &str) -> Option<NamedSets> {
        Some(named_sets(&self.0))
    }
}

/// The `checks-owed` state apply hook.
pub struct ChecksOwedHook {
    /// `task_checks.enabled` of this process, kept by [`register_hook`].
    enabled: AtomicBool,
    sets: Mutex<Option<Arc<dyn NamedSetsLookup>>>,
    /// Makes the next calls fail, for tests of the transaction's abort.
    #[cfg(test)]
    pub(crate) fail: AtomicBool,
}

impl Default for ChecksOwedHook {
    fn default() -> Self {
        Self::new()
    }
}

impl ChecksOwedHook {
    /// A hook with the setting off and no named sets.
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            sets: Mutex::new(None),
            #[cfg(test)]
            fail: AtomicBool::new(false),
        }
    }

    /// Follows `task_checks.enabled`: while it is off the hook only deletes.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Where the hook finds the named sets of a project.
    pub fn set_named_sets(&self, lookup: Arc<dyn NamedSetsLookup>) {
        *self
            .sets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(lookup);
    }

    fn named_sets_of(&self, definitions: &DefinitionTree) -> Option<NamedSets> {
        let project = definitions.project.as_ref()?.id.clone()?;
        let lookup = self
            .sets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()?;
        lookup.named_sets(&project)
    }

    /// Whether the leaf `task_id` owes checks when it finishes under `definitions`: its kind
    /// takes checks and a required ordinary check applies to it, or it uses a named set that
    /// cannot be read here.
    fn owes_checks(&self, task_id: &str, definitions: &DefinitionTree) -> bool {
        let sets = self.named_sets_of(definitions).unwrap_or_default();
        // Every other task counts as finished, so no closing check is carried: those are not
        // judged here yet.
        let set = effective_set(task_id, definitions, &sets, &|_| true);
        if matches!(
            set.applicability,
            Applicability::NoChecksApply | Applicability::UnknownTask
        ) {
            return false;
        }
        set.required()
            .any(|row| !matches!(row.source, Source::Closing { .. }))
            || uses_unknown_set(task_id, definitions, &sets)
    }
}

impl StateApplyHook for ChecksOwedHook {
    fn name(&self) -> &str {
        CHECKS_OWED_HOOK
    }

    fn apply(&self, tx: &mut StateTx, change: &StateApplyChange) -> Result<()> {
        #[cfg(test)]
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("the owed markers could not be written");
        }
        // Before the project's definitions are loaded no task can be marked, and a task missing
        // from an empty tree is not a deleted task. The deletes still run for a task the record
        // alone shows reopened.
        if change.definitions.top.is_none() {
            for touch in change.touched {
                if surely_reopened(touch.before.as_ref(), touch.after.as_ref()) {
                    delete_rows(tx, touch.id.as_str())?;
                }
            }
            return Ok(());
        }
        let enabled = self.is_enabled();
        for touch in change.touched {
            let id = touch.id.as_str();
            // Groups are not marked yet: a leaf that became one loses its old leaf rows.
            if is_group(change.definitions, id) {
                delete_rows(tx, id)?;
                continue;
            }
            if !done_looking(touch.definition_after, touch.after.as_ref()) {
                delete_rows(tx, id)?;
                continue;
            }
            if !enabled || done_looking(touch.definition_before, touch.before.as_ref()) {
                continue;
            }
            if self.owes_checks(id, change.definitions) {
                put_marker(tx, id, &OwedMarker::default())?;
                // A done record is only valid for the finish it judged: none survives a reopen,
                // so one found here is stale.
                delete_present(tx, DONE_NAMESPACE, id)?;
            }
        }
        // A leaf that gains children in a landing whose own definition did not change is not
        // touched; it is a group now, so its leaf rows go too.
        if let Some(old) = change.old_definitions {
            for node in change.definitions.nodes() {
                let id = node.definition.id.as_str();
                if is_group(change.definitions, id) && old.node(id).is_some() && !is_group(old, id)
                {
                    delete_rows(tx, id)?;
                }
            }
        }
        Ok(())
    }
}

/// Deletes the marker and the done record of `task_id`.
fn delete_rows(tx: &mut StateTx, task_id: &str) -> Result<()> {
    delete_present(tx, OWED_NAMESPACE, task_id)?;
    delete_present(tx, DONE_NAMESPACE, task_id)
}

/// Whether a write that took a task's record from `before` to `after` reopened it, judged from
/// the record alone so that no definition is needed: it has no state row now, it is cancelled,
/// or it is neither Done nor merged and a todo this write opened is open (one that was closed
/// before, or any open todo when the task was Done before). A todo the record has but the
/// definition may not have is not trusted alone.
fn surely_reopened(before: Option<&TaskStateRecord>, after: Option<&TaskStateRecord>) -> bool {
    let Some(after) = after else {
        return true;
    };
    if after.status == Status::Cancelled {
        return true;
    }
    let merged = after
        .result
        .as_ref()
        .is_some_and(|result| result.state == Some(ResultState::Merged));
    if after.status == Status::Done || merged {
        return false;
    }
    let Some(before) = before else {
        return false;
    };
    after.todos.iter().any(|todo| {
        todo.state == TodoState::Open
            && todo.id.is_some()
            && (before.status == Status::Done
                || before
                    .todos
                    .iter()
                    .any(|old| old.id == todo.id && old.state != TodoState::Open))
    })
}

/// Deletes `key` of `namespace` when it is there, so a transaction that changes no marker does
/// not name the namespace.
fn delete_present(tx: &mut StateTx, namespace: &str, key: &str) -> Result<()> {
    if tx.kv(namespace).get(key)?.is_some() {
        tx.kv(namespace).delete(key)?;
    }
    Ok(())
}

/// Whether `task_id` is the top group or has children in `definitions`.
fn is_group(definitions: &DefinitionTree, task_id: &str) -> bool {
    definitions
        .node(task_id)
        .is_some_and(|node| node.folder.is_empty() || !node.children.is_empty())
}

/// Whether the leaf with `definition` and state `record` is Done as the Work view derives it,
/// ignoring any hold. A task with no definition is not; a task with no state row is open.
pub(crate) fn done_looking(
    definition: Option<&TaskDefinition>,
    record: Option<&TaskStateRecord>,
) -> bool {
    let Some(definition) = definition else {
        return false;
    };
    let file = join(definition.clone(), record.cloned().unwrap_or_default());
    let task = Task {
        slug: String::new(),
        dir: PathBuf::new(),
        definition: None,
        pending: None,
        needs_conversion: false,
        done_hold: None,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    };
    derive::state(&task, &WorkTree::default()) == State::Done
}

/// Whether the task or a group above it uses a named set that `sets` does not have.
fn uses_unknown_set(task_id: &str, definitions: &DefinitionTree, sets: &NamedSets) -> bool {
    let mut chain = Vec::new();
    let Some(top) = &definitions.top else {
        return false;
    };
    if !find_chain(top, task_id, &mut chain) {
        return false;
    }
    chain.iter().any(|node| {
        node.definition.extra.get("checks").is_some_and(|value| {
            decl::parse(&node.folder, value)
                .uses
                .iter()
                .any(|name| sets.get(name).is_none())
        })
    })
}

fn find_chain<'a>(node: &'a DefinitionNode, id: &str, chain: &mut Vec<&'a DefinitionNode>) -> bool {
    chain.push(node);
    if node.definition.id == id
        || node
            .children
            .iter()
            .any(|child| find_chain(child, id, chain))
    {
        return true;
    }
    chain.pop();
    false
}

static HOOK: OnceLock<Arc<ChecksOwedHook>> = OnceLock::new();

/// The hook of this process, once [`register_hook`] ran.
pub fn global_hook() -> Option<Arc<ChecksOwedHook>> {
    HOOK.get().cloned()
}

/// Registers the hook on the global registry, once per process, and keeps it following
/// `task_checks.enabled`. Must run before the first project opens: the first writer seals the
/// registry. The hook is registered whether the setting is on or off, because its deletes run
/// either way.
pub fn register_hook(cx: &mut App) {
    let hook = HOOK.get_or_init(|| {
        let hook = Arc::new(ChecksOwedHook::new());
        StateApplyHooks::global().register(hook.clone());
        hook
    });
    hook.set_named_sets(Arc::new(OpenReposSets(OpenProjectRepos::global(cx))));
    hook.set_enabled(TaskChecksSettings::get_global(cx).enabled);
    cx.observe_global::<SettingsStore>(|cx| {
        if let Some(hook) = HOOK.get() {
            hook.set_enabled(TaskChecksSettings::get_global(cx).enabled);
        }
    })
    .detach();
}
