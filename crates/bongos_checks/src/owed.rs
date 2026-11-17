//! Which finished tasks owe checks: the `checks-owed` state apply hook. It runs at the end of
//! every writer transaction that writes task rows or moves the verified commit, before the
//! commit, and keeps one marker row per finished task that owes checks, `checks.owed/<task id>`,
//! written in the same transaction as the write that finished the task. No timestamps and no
//! sweep: a task is held while it has a marker and no valid done record, so a crash right after
//! the finishing write leaves it held, and nothing is recomputed at start.
//!
//! "Done-looking" is the Done the Work view would derive for the task if no hold existed: a leaf
//! whose todos are all closed or skipped, or whose status is Done; a group whose status is Done
//! or whose live (not cancelled) children are all Done-looking. For each task a transaction
//! touches, and each group above one (on a landing, every group):
//! - a leaf that becomes Done-looking while `task_checks.enabled` is on in this process, whose
//!   kind takes checks and whose required set (from the definitions in force after the
//!   transaction) is not empty, gets its marker. Its set includes the closing checks of each
//!   group above it for which it is the last open child: every other live child of that group
//!   was Done before this transaction and still is, owing nothing or holding a readable done
//!   record. A sibling that finishes in the same transaction counts as held, so of children
//!   finishing together none carries them. The marker's `closing_from` names those groups;
//! - any child that becomes Done-looking, whatever it owes and whether the setting is on, is
//!   recorded as `checks.last/<group id>` of its group when the group has a closing set (the
//!   last in child order when several finish together), with the next value of `checks.seq`;
//! - a group that becomes Done-looking while the setting is on and whose required closing set
//!   is not empty gets its marker, with `last_child` from its `checks.last` row (a recorded
//!   child that is no longer a live child of the group is ignored for the live child with the
//!   latest generation in its done record or its own `checks.last` row). A group's ordinary rows
//!   pass to its children and never mark it;
//! - a task that is not Done-looking afterward loses its marker and its done record, whether the
//!   setting is on or off, so a task reopened and finished again while it is off is not held
//!   later by an old marker; a leaf that becomes a group loses the rows of the leaf it was;
//! - any other task keeps or lacks its marker as it was: a check added to a task that is already
//!   finished never marks it, and turning the setting off and on changes nothing.
//!
//! Before the project's definitions are loaded nothing is marked, and the marker and done record
//! go only when the state record alone proves the task is not Done: its state row is gone, or it
//! is cancelled.
//!
//! The hook writes only `kv` rows, runs no git and no process, and any error aborts the whole
//! transaction, so no task is finished without its marker. Its one read outside the transaction
//! is the named check sets of the project's master checkout, which the definitions tree does not
//! carry; when they cannot be read and a task or a group above it uses one, the task is marked
//! (a group as having a closing set), and the decider judges it with the sets it reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};
use bongos_checks_model::{Applicability, NamedSets, Source, decl, effective_set};
use bongos_project_repo::{StateApplyChange, StateApplyHook, StateApplyHooks, StateTx, TaskTouch};
use bongos_project_repo_model::{
    DefinitionNode, DefinitionTree, TaskDefinition, TaskStateRecord, join,
};
use bongos_work::OpenProjectRepos;
use bongos_work::model::Status;
use bongos_work::model::derive::{self, State};
use bongos_work::model::store::{Task, WorkTree};
use gpui::App;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};

use crate::definitions::named_sets;
use crate::evidence_store::{
    DONE_NAMESPACE, DoneRecord, LAST_NAMESPACE, OWED_NAMESPACE, done_record_in, next_generation,
};
use crate::pin::master_definitions;
use crate::settings::TaskChecksSettings;

/// The name of the hook among the state apply hooks.
pub const CHECKS_OWED_HOOK: &str = "checks-owed";

/// The marker of a finished task that owes checks, `checks.owed/<task id>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwedMarker {
    /// The groups whose closing checks this task carries as their last open child.
    #[serde(default)]
    pub closing_from: Vec<String>,
    /// On a group's marker only: the last child to become Done-looking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_child: Option<LastChild>,
    /// The run state's generation of the transaction that put the marker. A task finished
    /// again gets a new one, so a decision taken for an earlier finish never lands on a later
    /// one. Zero on a marker written before it was kept.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub state_generation: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The last child of a group to become Done-looking, with the generation that orders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastChild {
    pub id: String,
    pub generation: u64,
}

/// The row `checks.last/<group id>` of a group with a closing set: the last of its children to
/// become Done-looking, recorded by the hook whatever that child owes and whether the setting
/// is on, with the generation that orders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRow {
    pub id: String,
    pub generation: u64,
    /// The tree key of the child's tree, filled in by the decider; empty until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_key: Option<String>,
    /// When the decider computed `tree_key` on the tree as it found it, later than the child's
    /// finish (after a crash, or while the setting was off): the time it did. Empty when the key
    /// was computed right after the child was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found_at: Option<String>,
}

/// Writes the `checks.last` row of `group_id`.
pub fn put_last(tx: &mut StateTx, group_id: &str, row: &LastRow) -> Result<()> {
    let value = toml::Value::try_from(row).context("serializing a last child row")?;
    tx.kv(LAST_NAMESPACE).put(group_id, value)
}

/// The `checks.last` row of `group_id` as `tx` sees it.
pub fn last_row_in(tx: &mut StateTx, group_id: &str) -> Result<Option<LastRow>> {
    tx.kv(LAST_NAMESPACE)
        .get(group_id)?
        .map(|value| {
            value
                .try_into()
                .with_context(|| format!("reading the last child of {group_id}"))
        })
        .transpose()
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

    /// The marker `task_id` gets when it finishes under `definitions`, `done` saying which
    /// other tasks are Done; `None` when it owes nothing. A leaf owes its required rows, the
    /// closing checks it carries as the last open child included; a group owes its required
    /// closing set and never its ordinary rows. A task that uses, or sits under a group that
    /// uses, a named set that cannot be read here owes checks. The marker names the groups
    /// whose closing checks the task carries; `last_child` is left to the caller.
    fn owed_marker(
        &self,
        task_id: &str,
        definitions: &DefinitionTree,
        sets: &NamedSets,
        done: &dyn Fn(&str) -> bool,
    ) -> Option<OwedMarker> {
        let set = effective_set(task_id, definitions, sets, done);
        if matches!(
            set.applicability,
            Applicability::NoChecksApply | Applicability::UnknownTask
        ) {
            return None;
        }
        if set.required().next().is_none() && !uses_unknown_set(task_id, definitions, sets) {
            return None;
        }
        let mut closing_from: Vec<String> = Vec::new();
        for row in set.judged() {
            if let Source::Closing { group } = &row.source
                && !closing_from.contains(group)
            {
                closing_from.push(group.clone());
            }
        }
        Some(OwedMarker {
            closing_from,
            last_child: None,
            state_generation: 0,
        })
    }

    /// Whether the group `group_id` has a closing set (its own closing checks and those passed
    /// down to it), or uses a named set that cannot be read here, which may hold some.
    fn has_closing_set(
        &self,
        group_id: &str,
        definitions: &DefinitionTree,
        sets: &NamedSets,
        done: &dyn Fn(&str) -> bool,
    ) -> bool {
        !effective_set(group_id, definitions, sets, done)
            .closing
            .is_empty()
            || uses_unknown_set(group_id, definitions, sets)
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
        // alone shows is not Done: its state row is gone, or it is cancelled.
        if change.definitions.top.is_none() {
            for touch in change.touched {
                if surely_not_done(touch.after.as_ref()) {
                    delete_rows(tx, touch.id.as_str())?;
                }
            }
            return Ok(());
        }
        let enabled = self.is_enabled();
        let mut view = View::new(change);

        // Which tasks became Done-looking, and which lost it.
        let mut finished = Vec::new();
        for id in view.candidates(change) {
            let before = view.looks_done(tx, Side::Before, &id)?;
            let after = view.looks_done(tx, Side::After, &id)?;
            // A leaf that became a group: its rows judged the leaf it was.
            if view.became_group(&id) {
                delete_rows(tx, &id)?;
            }
            if !after {
                delete_rows(tx, &id)?;
            } else if !before {
                finished.push(id);
            }
        }

        if finished.is_empty() {
            return Ok(());
        }
        // Read only when a task finishes: most transactions finish none. An unreadable set
        // counts as unknown, so a task that uses one is marked.
        let sets = self.named_sets_of(change.definitions).unwrap_or_default();

        // The last child of each group: of the children of one group that became Done-looking
        // here, the last in child order. Recorded whether the setting is on or off and whatever
        // the child owes, for every group with a closing set.
        let mut last_by_group: BTreeMap<String, (usize, String)> = BTreeMap::new();
        for id in &finished {
            let Some((parent, position)) = view.parent_and_position(id) else {
                continue;
            };
            let entry = last_by_group
                .entry(parent.definition.id.clone())
                .or_insert((position, id.clone()));
            if position >= entry.0 {
                *entry = (position, id.clone());
            }
        }
        for (group, (_, child)) in last_by_group {
            let done = view.done_map(tx, &group)?;
            let done = |other: &str| done.get(other).copied().unwrap_or(false);
            if !self.has_closing_set(&group, change.definitions, &sets, &done) {
                continue;
            }
            let generation = next_generation(tx)?;
            put_last(
                tx,
                &group,
                &LastRow {
                    id: child,
                    generation,
                    tree_key: None,
                    found_at: None,
                },
            )?;
        }

        if !enabled {
            return Ok(());
        }
        // The markers. A sibling that became Done-looking in this transaction is not Done for
        // its siblings (it is held), so of children finishing together none carries the
        // closing checks of their group; the group judges them itself.
        for id in &finished {
            let done = view.done_map(tx, id)?;
            let done = |other: &str| done.get(other).copied().unwrap_or(false);
            let Some(mut marker) = self.owed_marker(id, change.definitions, &sets, &done) else {
                continue;
            };
            if view.is_group(Side::After, id) {
                marker.last_child = view.last_child(tx, id)?;
            }
            // The generation this transaction commits as.
            marker.state_generation = tx.generation()? + 1;
            put_marker(tx, id, &marker)?;
            // A done record is only valid for the finish it judged: none survives a reopen, so
            // one found here is stale.
            delete_present(tx, DONE_NAMESPACE, id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    /// Before the transaction: the old verified commit's definitions and the rows before it.
    Before,
    /// After the transaction.
    After,
}

/// The nodes of one definitions tree by id, with the parent of each.
#[derive(Default)]
struct TreeIndex<'a> {
    nodes: HashMap<&'a str, &'a DefinitionNode>,
    parents: HashMap<&'a str, &'a DefinitionNode>,
}

impl<'a> TreeIndex<'a> {
    fn new(tree: &'a DefinitionTree) -> Self {
        let mut index = Self::default();
        if let Some(top) = &tree.top {
            index.add(top);
        }
        index
    }

    fn add(&mut self, node: &'a DefinitionNode) {
        self.nodes.insert(node.definition.id.as_str(), node);
        for child in &node.children {
            self.parents.insert(child.definition.id.as_str(), node);
            self.add(child);
        }
    }
}

/// What one transaction looks like before and after, for the hook: the Done-looking state of any
/// task on either side (a group's from its children), and whether a task is Done for the
/// question of who carries a group's closing checks. Rows of tasks the transaction did not touch
/// are read through it and are the same on both sides.
struct View<'a> {
    before: TreeIndex<'a>,
    after: TreeIndex<'a>,
    touched: HashMap<&'a str, &'a TaskTouch<'a>>,
    records: HashMap<String, Option<TaskStateRecord>>,
    looks: HashMap<(Side, String), bool>,
    cancelled: HashMap<(Side, String), bool>,
    done: HashMap<String, bool>,
}

impl<'a> View<'a> {
    fn new(change: &StateApplyChange<'a>) -> Self {
        Self {
            before: TreeIndex::new(change.old_definitions.unwrap_or(change.definitions)),
            after: TreeIndex::new(change.definitions),
            touched: change
                .touched
                .iter()
                .map(|touch| (touch.id.as_str(), touch))
                .collect(),
            records: HashMap::new(),
            looks: HashMap::new(),
            cancelled: HashMap::new(),
            done: HashMap::new(),
        }
    }

    fn index(&self, side: Side) -> &TreeIndex<'a> {
        match side {
            Side::Before => &self.before,
            Side::After => &self.after,
        }
    }

    fn node(&self, side: Side, id: &str) -> Option<&'a DefinitionNode> {
        self.index(side).nodes.get(id).copied()
    }

    fn is_group(&self, side: Side, id: &str) -> bool {
        self.node(side, id).is_some_and(is_group_node)
    }

    /// Whether `id` was a leaf before the transaction and is a group after it.
    fn became_group(&self, id: &str) -> bool {
        self.node(Side::Before, id)
            .is_some_and(|node| !is_group_node(node))
            && self.is_group(Side::After, id)
    }

    /// The tasks whose Done-looking state the transaction may change: the touched tasks and
    /// every group above them on either side, and on a landing every group (one may have lost
    /// or gained children without its own row or definition changing). In id order.
    fn candidates(&self, change: &StateApplyChange) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for touch in change.touched {
            ids.insert(touch.id.clone());
            for side in [Side::Before, Side::After] {
                let mut parent = self.index(side).parents.get(touch.id.as_str()).copied();
                while let Some(node) = parent {
                    let id = node.definition.id.as_str();
                    ids.insert(id.to_string());
                    parent = self.index(side).parents.get(id).copied();
                }
            }
        }
        if change.old_definitions.is_some() {
            for (id, node) in &self.after.nodes {
                if is_group_node(node) {
                    ids.insert(id.to_string());
                }
            }
        }
        ids
    }

    /// The parent of `id` after the transaction and `id`'s place among its children.
    fn parent_and_position(&self, id: &str) -> Option<(&'a DefinitionNode, usize)> {
        let parent = self.after.parents.get(id).copied()?;
        let position = parent
            .children
            .iter()
            .position(|child| child.definition.id == id)?;
        Some((parent, position))
    }

    fn record(&mut self, tx: &StateTx, side: Side, id: &str) -> Result<Option<TaskStateRecord>> {
        if let Some(touch) = self.touched.get(id) {
            return Ok(match side {
                Side::Before => touch.before.clone(),
                Side::After => touch.after.clone(),
            });
        }
        if let Some(record) = self.records.get(id) {
            return Ok(record.clone());
        }
        let record = tx.task(id)?;
        self.records.insert(id.to_string(), record.clone());
        Ok(record)
    }

    fn definition(&self, side: Side, id: &str) -> Option<&'a TaskDefinition> {
        match self.touched.get(id) {
            Some(touch) => match side {
                Side::Before => touch.definition_before,
                Side::After => touch.definition_after,
            },
            None => self.node(side, id).map(|node| &node.definition),
        }
    }

    /// Whether `id` is cancelled on `side`: its status says so, or it has children and all of
    /// them are.
    fn is_cancelled(&mut self, tx: &StateTx, side: Side, id: &str) -> Result<bool> {
        let key = (side, id.to_string());
        if let Some(cancelled) = self.cancelled.get(&key) {
            return Ok(*cancelled);
        }
        let Some(node) = self.node(side, id) else {
            return Ok(false);
        };
        let mut cancelled = self
            .record(tx, side, id)?
            .is_some_and(|record| record.status == Status::Cancelled);
        if !cancelled && !node.children.is_empty() {
            cancelled = true;
            for child in &node.children {
                if !self.is_cancelled(tx, side, &child.definition.id)? {
                    cancelled = false;
                    break;
                }
            }
        }
        self.cancelled.insert(key, cancelled);
        Ok(cancelled)
    }

    /// Whether `id` is Done on `side` as the Work view derives it, ignoring every hold: a leaf
    /// by its todos, status and result; a group not cancelled whose status is Done or that has
    /// live (not cancelled) children, all Done-looking. A task missing on that side is not.
    fn looks_done(&mut self, tx: &StateTx, side: Side, id: &str) -> Result<bool> {
        let key = (side, id.to_string());
        if let Some(looks) = self.looks.get(&key) {
            return Ok(*looks);
        }
        let Some(node) = self.node(side, id) else {
            return Ok(false);
        };
        let looks = if is_group_node(node) {
            self.group_looks_done(tx, side, node)?
        } else {
            let record = self.record(tx, side, id)?;
            done_looking(self.definition(side, id), record.as_ref())
        };
        self.looks.insert(key, looks);
        Ok(looks)
    }

    fn group_looks_done(
        &mut self,
        tx: &StateTx,
        side: Side,
        node: &'a DefinitionNode,
    ) -> Result<bool> {
        let id = node.definition.id.as_str();
        if self.is_cancelled(tx, side, id)? {
            return Ok(false);
        }
        if self
            .record(tx, side, id)?
            .is_some_and(|record| record.status == Status::Done)
        {
            return Ok(true);
        }
        let mut any_live = false;
        for child in &node.children {
            let child = child.definition.id.as_str();
            if self.is_cancelled(tx, side, child)? {
                continue;
            }
            any_live = true;
            if !self.looks_done(tx, side, child)? {
                return Ok(false);
            }
        }
        Ok(any_live)
    }

    /// Whether `id`, or a task below it, keeps it from being Done for its siblings: it became
    /// Done-looking in this transaction, or it has a marker and no readable done record.
    fn held(&mut self, tx: &mut StateTx, id: &str) -> Result<bool> {
        if self.looks_done(tx, Side::After, id)? && !self.looks_done(tx, Side::Before, id)? {
            return Ok(true);
        }
        if tx.kv(OWED_NAMESPACE).get(id)?.is_some() && !has_valid_done(tx, id)? {
            return Ok(true);
        }
        let Some(node) = self.node(Side::After, id) else {
            return Ok(false);
        };
        for child in &node.children {
            if self.held(tx, &child.definition.id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `id` is Done when a sibling asks who carries their group's closing checks: it
    /// is cancelled (not live), or it looked Done before this transaction and still does, and
    /// neither it nor a task below it is held.
    fn sibling_done(&mut self, tx: &mut StateTx, id: &str) -> Result<bool> {
        if let Some(done) = self.done.get(id) {
            return Ok(*done);
        }
        let done = if self.is_cancelled(tx, Side::After, id)? {
            true
        } else {
            self.looks_done(tx, Side::Before, id)?
                && self.looks_done(tx, Side::After, id)?
                && !self.held(tx, id)?
        };
        self.done.insert(id.to_string(), done);
        Ok(done)
    }

    /// Whether each child of every group above `id` is Done, for [`effective_set`]'s `done`
    /// (a task missing from the map is not Done). `id` itself is never Done here: it is
    /// finishing in this transaction.
    fn done_map(&mut self, tx: &mut StateTx, id: &str) -> Result<HashMap<String, bool>> {
        let mut map = HashMap::new();
        let mut parent = self.after.parents.get(id).copied();
        while let Some(node) = parent {
            for child in &node.children {
                let child = child.definition.id.as_str();
                let done = self.sibling_done(tx, child)?;
                map.insert(child.to_string(), done);
            }
            parent = self.after.parents.get(node.definition.id.as_str()).copied();
        }
        Ok(map)
    }

    /// The last child the marker of the group `group_id` names: its `checks.last` row when that
    /// child is still a live child of the group; when it is not (moved away or dropped), the
    /// live child with the highest generation in its done record or its own `checks.last`
    /// row. `None` when the group has no `checks.last` row.
    fn last_child(&mut self, tx: &mut StateTx, group_id: &str) -> Result<Option<LastChild>> {
        let Some(row) = readable_last_row(tx, group_id)? else {
            return Ok(None);
        };
        let Some(group) = self.node(Side::After, group_id) else {
            return Ok(None);
        };
        let mut live = Vec::new();
        for child in &group.children {
            let child = child.definition.id.as_str();
            if !self.is_cancelled(tx, Side::After, child)? {
                live.push(child);
            }
        }
        if live.contains(&row.id.as_str()) {
            return Ok(Some(LastChild {
                id: row.id,
                generation: row.generation,
            }));
        }
        let mut latest: Option<LastChild> = None;
        for child in live {
            let done = done_record_in(tx, child).ok().flatten();
            let last = readable_last_row(tx, child)?;
            let generation = done
                .map(|done| done.generation)
                .into_iter()
                .chain(last.map(|last| last.generation))
                .max();
            if let Some(generation) = generation
                && latest
                    .as_ref()
                    .is_none_or(|latest| generation > latest.generation)
            {
                latest = Some(LastChild {
                    id: child.to_string(),
                    generation,
                });
            }
        }
        Ok(latest)
    }
}

/// Whether `node` is a group: the top task or a task with children.
fn is_group_node(node: &DefinitionNode) -> bool {
    node.folder.is_empty() || !node.children.is_empty()
}

/// Whether `task_id` has a done record that reads as one.
fn has_valid_done(tx: &mut StateTx, task_id: &str) -> Result<bool> {
    Ok(tx
        .kv(DONE_NAMESPACE)
        .get(task_id)?
        .is_some_and(|value| value.try_into::<DoneRecord>().is_ok()))
}

/// The `checks.last` row of `group_id`; one that cannot be read counts as missing.
fn readable_last_row(tx: &mut StateTx, group_id: &str) -> Result<Option<LastRow>> {
    let Some(value) = tx.kv(LAST_NAMESPACE).get(group_id)? else {
        return Ok(None);
    };
    match value.try_into() {
        Ok(row) => Ok(Some(row)),
        Err(error) => {
            log::error!("reading the last child of {group_id}: {error}");
            Ok(None)
        }
    }
}

/// Deletes the marker and the done record of `task_id`.
fn delete_rows(tx: &mut StateTx, task_id: &str) -> Result<()> {
    delete_present(tx, OWED_NAMESPACE, task_id)?;
    delete_present(tx, DONE_NAMESPACE, task_id)
}

/// Whether the task whose record is now `after` surely no longer looks Done, judged from the
/// record alone so that no definition is needed: it has no state row, or it is cancelled. An
/// open todo or a status set back from Done proves nothing without the definition (it may have
/// dropped that todo, and its todos may all be closed), so neither counts.
fn surely_not_done(after: Option<&TaskStateRecord>) -> bool {
    after.is_none_or(|after| after.status == Status::Cancelled)
}

/// Deletes `key` of `namespace` when it is there, so a transaction that changes no marker does
/// not name the namespace.
fn delete_present(tx: &mut StateTx, namespace: &str, key: &str) -> Result<()> {
    if tx.kv(namespace).get(key)?.is_some() {
        tx.kv(namespace).delete(key)?;
    }
    Ok(())
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
pub(crate) fn uses_unknown_set(
    task_id: &str,
    definitions: &DefinitionTree,
    sets: &NamedSets,
) -> bool {
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
