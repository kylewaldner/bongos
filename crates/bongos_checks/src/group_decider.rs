//! The done decider's half for groups. A group is marked only when its closing set is not
//! empty, and its marker names its last child. It is Done, and gets its done record
//! `{ tree_key, generation, at, evidence }`, when every live child owes nothing or has a valid
//! done record and every required closing check has a fresh pass on the last child's tree.
//!
//! That tree is named by a tree key: the last child's done record's key (an empty key is no
//! key), or, for a child that owes nothing, the key the decider adds to the group's
//! `checks.last` row when it sees the child recorded (the hook writes only `kv` rows, so it
//! cannot compute one). For a child that is itself a group, its own last child's tree, down to a
//! leaf. A key computed right when the child is recorded is the tree as it finished; one computed
//! later (the child was recorded before this decider followed the project: a crash, the setting
//! off) is the tree as found then, and the group's reason says so. With no tree found the key
//! stays empty and the group shows that the tree is gone.
//!
//! When the last child carried the closing checks, its done record already holds their passes,
//! and the group's record rests on it. Otherwise the decider starts the missing closing checks
//! itself, started by "group decision", at most once per (group, check, digest, tree key): after
//! that run a fail or no verdict on the key waits for a person's run or a new key, and
//! re-decisions start nothing. The run is on the recorded tree, not a live worktree: each
//! repository's tree object is checked out in the second tree the lane of the last child lends
//! (with that tree's environment, the lease released after the run), waiting for a busy one up
//! to the pool wait limit; with none it is written out cold under the scratch folder, with
//! bongos's own environment, and removed after the run.
//!
//! Every task under a group with closing checks whose done record or `checks.last` key the
//! decider writes gets a ref keeping its tree ([`crate::retained`]); refs no record backs any
//! more, or whose groups with closing checks are all Done, are deleted on the first decision and
//! after each change of the verified commit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bongos_checks_model::evidence::{Current, StartedBy, satisfied};
use bongos_checks_model::{Row, effective_set};
use bongos_project_repo::{ProjectRepo, StateRead, StateWriter};
use bongos_project_repo_model::DefinitionTree;
use futures::FutureExt as _;
use futures::channel::oneshot;
use gpui::{AsyncApp, Context, WeakEntity};
use settings::Settings as _;

use crate::base_tree::{LentTreeError, TempTree, check_out_tree_in_lent_tree};
use crate::decider::{
    Decision, GroupRunHold, Judgment, Pinned, TreeKeys, evidence_of, find_chain, leaf_set,
    missing_reason, still_deciding, write_done_if,
};
use crate::engine::{
    ChecksEngine, Leases, RecordedTree, RecordedTrees, RunChecksRequest, check_digest,
    recorded_key, repo_trees,
};
use crate::evidence_store::{
    DONE_NAMESPACE, DoneRecord, LAST_NAMESPACE, OWED_NAMESPACE, done_record_in, scan_records,
    scan_records_in, task_prefix,
};
use crate::live::HoldRow;
use crate::owed::{LastChild, LastRow, OwedMarker, last_row_in, put_last, uses_unknown_set};
use crate::process::StopHandle;
use crate::retained::{KeptTree, delete_ref, has_tree, keep_trees, kept_tree, list_refs};
use crate::settings::TaskChecksSettings;
use crate::task_git::TaskGit;
use crate::task_source::{TREE_KEYS_FOLDER, TaskChecks};
use crate::trees::{EnvBase, SecondTree, SecondTreeLease, TaskTreesRegistry};

/// "not done: the tree of <child> is gone; reopen it".
pub fn tree_gone(child: &str) -> String {
    format!("not done: the tree of {child} is gone; reopen it")
}

/// "closing checks run on <child>'s tree as found at <time>, not as it finished".
pub fn as_found(child: &str, at: &str) -> String {
    format!("closing checks run on {child}'s tree as found at {at}, not as it finished")
}

/// The reason of a group whose live children are not all Done yet.
pub const CHILDREN_HELD: &str = "not done: waiting for the checks of its children";

/// The reason of a group whose marker names no last child.
pub const NO_LAST_CHILD: &str = "not done: no finished child to run its closing checks on";

/// The readable `checks.last` rows of a store, by group id.
pub(crate) fn last_rows(read: &StateRead) -> Result<Vec<(String, LastRow)>> {
    Ok(read
        .kv(LAST_NAMESPACE)
        .scan("")?
        .into_iter()
        .filter_map(|(group, value)| match value.try_into::<LastRow>() {
            Ok(row) => Some((group, row)),
            Err(error) => {
                log::error!("reading the last child of {group}: {error}");
                None
            }
        })
        .collect())
}

/// What a group decision reads of the store, once.
#[derive(Default)]
struct Snapshot {
    done: BTreeMap<String, DoneRecord>,
    owed: BTreeMap<String, OwedMarker>,
    last: BTreeMap<String, LastRow>,
}

fn snapshot(read: &StateRead) -> Result<Snapshot> {
    let mut snapshot = Snapshot::default();
    for (id, value) in read.kv(DONE_NAMESPACE).scan("")? {
        if let Ok(done) = value.try_into() {
            snapshot.done.insert(id, done);
        }
    }
    for (id, value) in read.kv(OWED_NAMESPACE).scan("")? {
        if let Ok(marker) = value.try_into() {
            snapshot.owed.insert(id, marker);
        }
    }
    snapshot.last = last_rows(read)?.into_iter().collect();
    Ok(snapshot)
}

fn is_group(tree: &DefinitionTree, id: &str) -> bool {
    tree.node(id)
        .is_some_and(|node| node.folder.is_empty() || !node.children.is_empty())
}

/// The groups above `task_id` with closing checks of their own, nearest last.
fn closing_ancestors(task_id: &str, pinned: &Pinned) -> Vec<String> {
    let mut chain = Vec::new();
    if let Some(top) = &pinned.tree.top {
        find_chain(top, task_id, &mut chain);
    }
    chain.pop();
    chain
        .into_iter()
        .map(|node| node.definition.id.clone())
        .filter(|id| {
            !effective_set(id, &pinned.tree, &pinned.sets, &|_| false)
                .closing
                .is_empty()
        })
        .collect()
}

/// Whether a group above `task_id` has closing checks: its tree is kept when it is decided.
pub(crate) fn has_closing_ancestor(task_id: &str, pinned: &Pinned) -> bool {
    !closing_ancestors(task_id, pinned).is_empty()
}

/// The repositories the required rows of `rows` run in, `own` for a check that names none, in
/// order of first use.
fn repos_of(rows: &[Row], own: &str) -> Vec<String> {
    let mut repos: Vec<String> = Vec::new();
    for row in rows.iter().filter(|row| row.decl.is_required()) {
        let named = row
            .decl
            .repos
            .clone()
            .filter(|repos| !repos.is_empty())
            .unwrap_or_else(|| vec![own.to_string()]);
        for repo in named {
            if !repos.contains(&repo) {
                repos.push(repo);
            }
        }
    }
    repos
}

/// A child's tree key computed now.
enum Computed {
    Key {
        key: String,
        /// Each repository's name, tree directory and tree id.
        trees: Vec<(String, PathBuf, String)>,
    },
    /// The task source does not know the child yet.
    Unknown,
    NoTree,
    Failed(String),
}

/// The key of `child`'s tree now, over the repositories `group`'s closing checks run in.
async fn compute_child_key(
    this: &WeakEntity<ChecksEngine>,
    project: &str,
    pinned: &Pinned,
    group: &str,
    child: &str,
    cx: &mut AsyncApp,
) -> Result<Computed> {
    let context = this.update(cx, |engine, cx| {
        let task = engine
            .task(child, cx)
            .filter(|task| task.project.location().id == project)?;
        let tree = engine.task_tree(&task, cx);
        Some((task, tree, TaskTreesRegistry::get(cx)))
    })?;
    let Some((task, tree, trees)) = context else {
        return Ok(Computed::Unknown);
    };
    let Some(tree) = tree else {
        return Ok(Computed::NoTree);
    };
    let rows = leaf_set(group, &pinned.tree, &pinned.sets, &[])
        .judged()
        .to_vec();
    let repos = repos_of(&rows, task.repo.as_deref().unwrap_or_default());
    if repos.is_empty() {
        return Ok(Computed::NoTree);
    }
    let Ok(repo_trees) = repo_trees(&task, &tree.path, trees.as_ref(), repos) else {
        return Ok(Computed::NoTree);
    };
    let mut keys = TreeKeys::default();
    let key = match keys
        .key(&TaskGit::of(&task.project), &task.index_dir, &repo_trees)
        .await
    {
        Ok(key) => key,
        Err(Judgment::Failed(why)) => return Ok(Computed::Failed(why)),
        Err(_) => return Ok(Computed::NoTree),
    };
    let trees = repo_trees
        .iter()
        .filter_map(|tree| {
            let id = keys.tree_of(&tree.repo)?;
            Some((
                tree.repo.clone(),
                tree.path.clone(),
                id.as_str().to_string(),
            ))
        })
        .collect();
    Ok(Computed::Key { key, trees })
}

async fn keep(git: &TaskGit, task_id: &str, trees: &[(String, PathBuf, String)]) {
    let kept: Vec<KeptTree> = trees
        .iter()
        .map(|(repo, directory, tree)| KeptTree {
            repo,
            directory,
            tree,
        })
        .collect();
    if let Err(error) = keep_trees(git, task_id, &kept).await {
        log::error!("keeping the tree of {task_id}: {error:#}");
    }
}

/// Writes `key` into the `checks.last` row of `group` in one transaction, if the row is still
/// `row` (none: still absent). Returns whether it wrote.
fn store_last_key(
    writer: &StateWriter,
    group: &str,
    row: Option<&LastRow>,
    filled: LastRow,
) -> Result<bool> {
    writer.transact(|tx| {
        if last_row_in(tx, group).ok().flatten().as_ref() != row {
            return Ok(false);
        }
        put_last(tx, group, &filled)?;
        Ok(true)
    })
}

/// Fills the tree key of every `checks.last` row without one whose child is a leaf that owes
/// nothing (a child that owes checks gets its key in its done record; a group's is its own last
/// child's). `fresh` holds the rows recorded since the last decision, whose key is the tree as
/// the child finished; any other is the tree as found now. A child with no tree keeps an empty
/// key.
pub(crate) async fn fill_last_keys(
    this: &WeakEntity<ChecksEngine>,
    project: &str,
    writer: &Arc<StateWriter>,
    pinned: &Pinned,
    rows: &[(String, LastRow)],
    fresh: &BTreeSet<(String, u64)>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let owed: BTreeSet<String> = writer.read(|read| {
        Ok(read
            .kv(OWED_NAMESPACE)
            .scan("")?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    })?;
    for (group, row) in rows {
        if row.tree_key.is_some()
            || owed.contains(&row.id)
            || pinned.tree.node(&row.id).is_none()
            || is_group(&pinned.tree, &row.id)
        {
            continue;
        }
        match compute_child_key(this, project, pinned, group, &row.id, cx).await? {
            Computed::Key { key, trees } => {
                if !still_deciding(this, cx)? {
                    return Ok(());
                }
                let found_at = if fresh.contains(&(group.clone(), row.generation)) {
                    None
                } else {
                    Some(this.update(cx, |engine, _| (engine.now)())?)
                };
                let git = this.update(cx, |engine, cx| {
                    engine
                        .task(&row.id, cx)
                        .map(|task| TaskGit::of(&task.project))
                })?;
                if let Some(git) = git {
                    keep(&git, &row.id, &trees).await;
                }
                let filled = LastRow {
                    tree_key: Some(key),
                    found_at,
                    ..row.clone()
                };
                store_last_key(writer, group, Some(row), filled)?;
            }
            Computed::Unknown => {
                this.update(cx, |engine, _| {
                    engine.decider.waiting.insert(project.to_string());
                })?;
            }
            Computed::NoTree => {}
            Computed::Failed(why) => {
                log::warn!("the tree key of {} could not be computed: {why}", row.id);
            }
        }
    }
    Ok(())
}

/// Where a group's last child's tree key comes from.
struct ChildKey {
    /// The leaf whose tree it is.
    leaf: String,
    /// The group whose `checks.last` row names `leaf`, and the generation it has there.
    parent: String,
    generation: u64,
    /// The key and, when it was computed on the tree as found, when.
    key: Option<(String, Option<String>)>,
}

/// The tree key of `child`, the last child of `group`: a leaf's done record key (empty is no
/// key), else its key in `group`'s `checks.last` row; a group's own last child's, down to a leaf.
fn child_key(
    snapshot: &Snapshot,
    tree: &DefinitionTree,
    group: &str,
    child: &LastChild,
    depth: usize,
) -> ChildKey {
    let none = || ChildKey {
        leaf: child.id.clone(),
        parent: group.to_string(),
        generation: child.generation,
        key: None,
    };
    if is_group(tree, &child.id) {
        if depth > 64 {
            return none();
        }
        let next = snapshot
            .owed
            .get(&child.id)
            .and_then(|marker| marker.last_child.clone())
            .or_else(|| {
                snapshot.last.get(&child.id).map(|row| LastChild {
                    id: row.id.clone(),
                    generation: row.generation,
                })
            });
        return match next {
            Some(next) => child_key(snapshot, tree, &child.id, &next, depth + 1),
            None => none(),
        };
    }
    if let Some(done) = snapshot.done.get(&child.id)
        && !done.tree_key.is_empty()
    {
        return ChildKey {
            key: Some((done.tree_key.clone(), None)),
            ..none()
        };
    }
    if let Some(row) = snapshot.last.get(group)
        && row.id == child.id
        && let Some(key) = row.tree_key.clone().filter(|key| !key.is_empty())
    {
        return ChildKey {
            key: Some((key, row.found_at.clone())),
            ..none()
        };
    }
    none()
}

/// The recorded trees of `key`, the key of `leaf`'s tree over `repos`: one tree's id is the key;
/// several are read from `leaf`'s refs. Each repository's object store must hold its tree. `None`
/// when the tree is gone.
async fn recorded_trees(
    git: &TaskGit,
    key: &str,
    leaf: &str,
    own: &str,
    repos: &[String],
    directories: &BTreeMap<String, PathBuf>,
) -> Option<RecordedTrees> {
    let mut trees = Vec::with_capacity(repos.len());
    for repo in repos {
        let directory = directories.get(repo)?;
        let tree = match repos {
            [_] => key.to_string(),
            _ => kept_tree(git, directory, leaf, Some(repo)).await?,
        };
        if !has_tree(git, directory, &tree).await {
            return None;
        }
        trees.push(RecordedTree {
            repo: repo.clone(),
            repository: directory.clone(),
            tree,
            path: PathBuf::new(),
            env: EnvBase::Own,
        });
    }
    let combined = recorded_key(
        trees
            .iter()
            .map(|tree| (tree.repo.as_str(), tree.tree.as_str())),
    );
    (combined == key).then(|| RecordedTrees {
        own: own.to_string(),
        trees,
    })
}

/// Decides the group `group_id` with `marker`: its done record when its closing checks pass on
/// its last child's tree (or that child carried them and is Done), else the reason it stays
/// held, starting the missing closing checks once per key.
pub(crate) async fn decide_group(
    this: &WeakEntity<ChecksEngine>,
    project: &str,
    writer: &Arc<StateWriter>,
    pinned: &Pinned,
    group_id: &str,
    marker: &OwedMarker,
    cx: &mut AsyncApp,
) -> Result<()> {
    let hold = |reason: String| HoldRow::new(reason);
    let write_hold = |this: &WeakEntity<ChecksEngine>, row: HoldRow, cx: &mut AsyncApp| {
        this.update(cx, |engine, cx| {
            engine.write_hold(project, writer, group_id, Some(row), cx)
        })
    };
    let done_written = |this: &WeakEntity<ChecksEngine>, cx: &mut AsyncApp| {
        this.update(cx, |engine, cx| engine.done_written(project, group_id, cx))
    };
    if uses_unknown_set(group_id, &pinned.tree, &pinned.sets) {
        let reason = "not done: a check set it uses could not be read".to_string();
        return write_hold(this, hold(reason), cx);
    }
    let rows = leaf_set(group_id, &pinned.tree, &pinned.sets, &marker.closing_from)
        .judged()
        .to_vec();
    let now = this.update(cx, |engine, _| engine.now.clone())?;
    if !rows.iter().any(|row| row.decl.is_required()) {
        let decision = Decision {
            task_id: group_id,
            marker,
            verified: &pinned.verified,
            rows: &rows,
            current: &Current::default(),
            evidence: &[],
            tree_key: "",
            at: now(),
        };
        if still_deciding(this, cx)? && write_done_if(writer, &decision, |_| Ok(true))? {
            done_written(this, cx)?;
        }
        return Ok(());
    }
    let snapshot = writer.read(snapshot)?;
    // Every live child owes nothing or is Done. A cancelled child has no marker.
    let children_done = pinned.tree.node(group_id).is_some_and(|node| {
        node.children.iter().all(|child| {
            let id = &child.definition.id;
            !snapshot.owed.contains_key(id) || snapshot.done.contains_key(id)
        })
    });
    if !children_done {
        return write_hold(this, hold(CHILDREN_HELD.to_string()), cx);
    }
    let Some(last) = marker.last_child.clone() else {
        return write_hold(this, hold(NO_LAST_CHILD.to_string()), cx);
    };

    // The last child carried the closing checks: its done record holds their passes.
    if let (Some(child_marker), Some(child_done)) =
        (snapshot.owed.get(&last.id), snapshot.done.get(&last.id))
        && child_marker
            .closing_from
            .iter()
            .any(|group| group == group_id)
    {
        let decision = Decision {
            task_id: group_id,
            marker,
            verified: &pinned.verified,
            rows: &rows,
            current: &Current::default(),
            evidence: &child_done.evidence,
            tree_key: &child_done.tree_key,
            at: now(),
        };
        let child = last.id.clone();
        let expected = child_done.clone();
        if still_deciding(this, cx)?
            && write_done_if(writer, &decision, |tx| {
                Ok(done_record_in(tx, &child).ok().flatten().as_ref() == Some(&expected))
            })?
        {
            done_written(this, cx)?;
        }
        return Ok(());
    }

    let group_task = this.update(cx, |engine, cx| {
        engine
            .task(group_id, cx)
            .filter(|task| task.project.location().id == project)
    })?;
    let Some(group_task) = group_task else {
        this.update(cx, |engine, _| {
            engine.decider.waiting.insert(project.to_string());
        })?;
        return Ok(());
    };
    let git = TaskGit::of(&group_task.project);

    // The last child's tree key; computed now, on the tree as found, when there is none.
    let mut found = child_key(&snapshot, &pinned.tree, group_id, &last, 0);
    let leaf_task = this.update(cx, |engine, cx| engine.task(&found.leaf, cx))?;
    let leaf_name = leaf_task
        .as_ref()
        .map_or_else(|| found.leaf.clone(), |task| task.name.clone());
    if found.key.is_none() {
        match compute_child_key(this, project, pinned, &found.parent, &found.leaf, cx).await? {
            Computed::Key { key, trees } => {
                let at = now();
                keep(&git, &found.leaf, &trees).await;
                let read = snapshot.last.get(&found.parent);
                let filled = LastRow {
                    id: found.leaf.clone(),
                    generation: found.generation,
                    tree_key: Some(key.clone()),
                    found_at: Some(at.clone()),
                };
                if still_deciding(this, cx)? {
                    store_last_key(writer, &found.parent, read, filled)?;
                }
                found.key = Some((key, Some(at)));
            }
            Computed::Unknown => {
                this.update(cx, |engine, _| {
                    engine.decider.waiting.insert(project.to_string());
                })?;
                return Ok(());
            }
            Computed::NoTree => {
                return write_hold(this, hold(tree_gone(&leaf_name)), cx);
            }
            Computed::Failed(why) => {
                return write_hold(this, hold(format!("not done: {why}")), cx);
            }
        }
    }
    let Some((key, found_at)) = found.key.clone() else {
        return write_hold(this, hold(tree_gone(&leaf_name)), cx);
    };
    let note = found_at.as_deref().map(|at| as_found(&leaf_name, at));
    let with_note = |reason: String| match &note {
        Some(note) => format!("{reason}; {note}"),
        None => reason,
    };

    // Each repository's tree, in the repository whose object store holds it.
    let own = leaf_task
        .as_ref()
        .and_then(|task| task.repo.clone())
        .unwrap_or_default();
    let repos = repos_of(&rows, &own);
    let (directories, primaries) = this.update(cx, |engine, cx| {
        let mut directories = BTreeMap::new();
        let mut primaries = Vec::new();
        let leaf_tree = leaf_task
            .as_ref()
            .and_then(|task| engine.task_tree(task, cx));
        for repo in &repos {
            let primary = engine.source.code_repository(repo, cx);
            primaries.extend(primary.clone());
            let directory = primary.or_else(|| {
                (repo == &own)
                    .then(|| leaf_tree.as_ref().map(|tree| tree.path.clone()))
                    .flatten()
            });
            if let Some(directory) = directory {
                directories.insert(repo.clone(), directory);
            }
        }
        (directories, primaries)
    })?;
    let git = git.with_primaries(primaries);
    let Some(recorded) = recorded_trees(&git, &key, &found.leaf, &own, &repos, &directories).await
    else {
        return write_hold(this, hold(with_note(tree_gone(&leaf_name))), cx);
    };

    // How each closing check stands on that tree.
    let mut current = Current::default();
    for row in &rows {
        let repos = row
            .decl
            .repos
            .clone()
            .filter(|repos| !repos.is_empty())
            .unwrap_or_else(|| vec![own.clone()]);
        let state = recorded
            .state(&git, &repos, &row.decl.key_inputs)
            .await
            .map_err(|reason| reason.text())
            .and_then(|(_, key, inputs)| {
                let digest = check_digest(&row.decl, &pinned.pin).map_err(|reason| {
                    format!("{} could not be read ({})", row.decl.name, reason.text())
                })?;
                Ok(bongos_checks_model::evidence::CheckState::new(
                    &key, &inputs, &digest,
                ))
            });
        match state {
            Ok(state) => {
                current.checks.insert(row.decl.name.clone(), state);
            }
            Err(_) if !row.decl.is_required() => {}
            Err(why) => return write_hold(this, hold(with_note(format!("not done: {why}"))), cx),
        }
    }
    let records = writer.read(|read| scan_records(read, &task_prefix(group_id)))?;
    let satisfaction = satisfied(&rows, &records, &current, group_id);
    if satisfaction.is_satisfied() {
        let evidence = evidence_of(&rows, &records, &current, group_id);
        if has_closing_ancestor(group_id, pinned) {
            let trees: Vec<(String, PathBuf, String)> = recorded
                .trees
                .iter()
                .map(|tree| {
                    (
                        tree.repo.clone(),
                        tree.repository.clone(),
                        tree.tree.clone(),
                    )
                })
                .collect();
            keep(&git, group_id, &trees).await;
        }
        let decision = Decision {
            task_id: group_id,
            marker,
            verified: &pinned.verified,
            rows: &rows,
            current: &current,
            evidence: &evidence,
            tree_key: &key,
            at: now(),
        };
        if still_deciding(this, cx)?
            && write_done_if(writer, &decision, |tx| {
                let records = scan_records_in(tx, &task_prefix(group_id))?;
                Ok(
                    satisfied(&rows, &records, &current, group_id).is_satisfied()
                        && evidence_of(&rows, &records, &current, group_id) == evidence,
                )
            })?
        {
            done_written(this, cx)?;
        }
        return Ok(());
    }

    // The missing closing checks run once per key; a fail or no verdict then waits.
    let unsatisfied: BTreeSet<&String> = [
        &satisfaction.missing,
        &satisfaction.failing,
        &satisfaction.no_verdict,
        &satisfaction.stale,
    ]
    .into_iter()
    .flatten()
    .collect();
    let (running, started, blocked) = this.update(cx, |engine, _| {
        (
            engine.is_running(group_id) || engine.decider.group_runs.contains_key(group_id),
            engine.decider.started.clone(),
            engine.decider.blocked.get(group_id).cloned(),
        )
    })?;
    let mut to_run: Vec<(String, String, String)> = Vec::new();
    for name in unsatisfied {
        let Some(state) = current.checks.get(name) else {
            continue;
        };
        let attempted = records.iter().any(|record| {
            &record.check == name
                && record.is_finished()
                && record.tree_key == state.tree_key
                && record.digest == state.digest
        }) || started.contains(&(
            group_id.to_string(),
            name.clone(),
            state.digest.clone(),
            state.tree_key.clone(),
        ));
        if !attempted {
            to_run.push((name.clone(), state.digest.clone(), state.tree_key.clone()));
        }
    }
    let count = satisfaction.unsatisfied_count();
    let row = match blocked {
        Some(why) => hold(with_note(format!(
            "not done: the tree of {leaf_name} could not be written out ({why})"
        ))),
        None => HoldRow {
            reason: with_note(missing_reason(count)),
            unsatisfied: Some(u32::try_from(count).unwrap_or(u32::MAX)),
        },
    };
    write_hold(this, row, cx)?;
    if !to_run.is_empty() && !running && still_deciding(this, cx)? {
        let leaf = found.leaf.clone();
        this.update(cx, |engine, cx| {
            engine.start_group_run(project, group_task, &leaf, recorded, to_run, cx)
        })?;
    }
    Ok(())
}

/// Forgets that `started` were started, when their run never began: the group runs them again
/// once the decider decides it (after an undrain, say).
fn forget_started(engine: &mut ChecksEngine, started: &[(String, String, String, String)]) {
    for entry in started {
        engine.decider.started.remove(entry);
    }
}

/// How often a closing run waiting for a busy second tree asks for it again.
pub(crate) const SECOND_TREE_RETRY: Duration = Duration::from_secs(15);

/// The second tree each repository of `repos` lends for `leaf` now, in order: `None` runs that
/// repository cold. While any repository answers busy, every lease taken is given back and the
/// question is asked again every [`SECOND_TREE_RETRY`] on the engine's clock, so a lease is
/// taken only once the lane has released it; past `task_checks.pool_wait_limit_hours` a busy
/// repository runs cold. With no tree provider, every repository runs cold at once. `Err` when
/// the engine is drained, no longer deciding or gone while waiting: nothing runs.
async fn second_trees(
    this: &WeakEntity<ChecksEngine>,
    leaf: &str,
    repos: &[String],
    cx: &mut AsyncApp,
) -> Result<Vec<Option<Box<dyn SecondTreeLease>>>> {
    let mut deadline: Option<Instant> = None;
    loop {
        let (trees, clock, limit) = this.update(cx, |engine, cx| {
            // Asked on every retry: a drain or the setting going off while waiting starts nothing.
            if !engine.deciding(cx) {
                return Err(anyhow::anyhow!(
                    "the engine is drained or task checks are off"
                ));
            }
            let hours = TaskChecksSettings::get_global(cx).pool_wait_limit_hours;
            Ok((
                TaskTreesRegistry::get(cx),
                engine.clock.clone(),
                Duration::from_secs(u64::from(hours) * 3600),
            ))
        })??;
        let Some(trees) = trees else {
            return Ok(repos.iter().map(|_| None).collect());
        };
        let now = clock.now();
        let deadline = *deadline.get_or_insert(now + limit);
        let mut busy = false;
        let mut answers = Vec::with_capacity(repos.len());
        for repo in repos {
            answers.push(match trees.second_tree(leaf, repo) {
                SecondTree::Lease(lease) => Some(lease),
                SecondTree::Busy => {
                    busy = true;
                    None
                }
                SecondTree::Unavailable => None,
            });
        }
        if !busy || now >= deadline {
            return Ok(answers);
        }
        // Given back before the wait, so no lane tree is held while another is awaited.
        drop(answers);
        clock
            .sleep(SECOND_TREE_RETRY.min(deadline.saturating_duration_since(now)))
            .await;
    }
}

impl ChecksEngine {
    /// Starts `checks` (name, digest, tree key) of the group `group` on `recorded`, the tree of
    /// its last child `leaf`, as a run started by "group decision". Each repository's tree is
    /// checked out in the second tree a lane lends for `leaf` (run with that tree's
    /// environment, waiting for a busy one up to the pool wait limit), else written out cold
    /// under the scratch folder (run with bongos's own environment). When the run ends, however
    /// it ends, every lease is released and every scratch folder removed; a drain stops the run
    /// and releases its leases at the latest when it resolves. Each (group, check,
    /// digest, key) is started at most once here.
    pub(crate) fn start_group_run(
        &mut self,
        project: &str,
        group: TaskChecks,
        leaf: &str,
        recorded: RecordedTrees,
        checks: Vec<(String, String, String)>,
        cx: &mut Context<Self>,
    ) {
        let id = group.task_id.clone();
        let entries: Vec<(String, String, String, String)> = checks
            .iter()
            .map(|(name, digest, key)| (id.clone(), name.clone(), digest.clone(), key.clone()))
            .collect();
        self.decider.started.extend(entries.iter().cloned());
        self.decider.blocked.remove(&id);
        let names: Vec<String> = checks.into_iter().map(|(name, _, _)| name).collect();
        let scratch = TaskChecksSettings::get_global(cx).scratch_folder();
        let index_dir = scratch.join(TREE_KEYS_FOLDER);
        let git = TaskGit::of(&group.project);
        let project = project.to_string();
        let leaf = leaf.to_string();
        // A lent tree is used only apart from every tree a person or a lane works in: the
        // trees of the group and of its last child, the project's repository and each code
        // repository the recorded trees come from.
        let mut protected: Vec<PathBuf> = vec![group.project.location().repository.clone()];
        protected.extend(recorded.trees.iter().map(|tree| tree.repository.clone()));
        if let Some(tree) = self.task_tree(&group, cx) {
            protected.push(tree.path);
        }
        if let Some(tree) = self
            .task(&leaf, cx)
            .and_then(|task| self.task_tree(&task, cx))
        {
            protected.push(tree.path);
        }
        let (ended, ended_receiver) = oneshot::channel::<()>();
        self.decider
            .group_runs
            .insert(id.clone(), ended_receiver.shared());
        // Held until the run has ended, and released on every path out; a drain stops the run
        // and releases them at the latest when it resolves.
        let stop = StopHandle::default();
        let leases: Leases = Rc::default();
        self.decider.group_holds.insert(
            id.clone(),
            GroupRunHold {
                stop: stop.clone(),
                leases: leases.clone(),
            },
        );
        cx.spawn(async move |this, cx| {
            let mut recorded = recorded;
            let mut temps: Vec<TempTree> = Vec::new();
            let mut failure = None;
            let mut stopped = false;
            let repos: Vec<String> = recorded.trees.iter().map(|tree| tree.repo.clone()).collect();
            let lent = match second_trees(&this, &leaf, &repos, cx).await {
                Ok(lent) => lent,
                Err(error) => {
                    log::info!("the closing checks of {id} did not start: {error:#}");
                    this.update(cx, |engine, _| {
                        engine.decider.group_runs.remove(&id);
                        engine.decider.group_holds.remove(&id);
                        forget_started(engine, &entries);
                    })
                    .ok();
                    drop(ended);
                    return;
                }
            };
            // Every lease is registered at once (no await since they were taken), so a drain
            // releases each. Only the one whose tree git is checking out is taken out of the
            // drain's reach, and only while git runs in it: registered again once its checkout
            // returned, dropped on every other path out.
            let lent: Vec<Option<(PathBuf, EnvBase)>> = lent
                .into_iter()
                .map(|lease| {
                    lease.map(|lease| {
                        let found = (lease.path().to_path_buf(), EnvBase::for_lease(lease.as_ref()));
                        leases.borrow_mut().push(lease);
                        found
                    })
                })
                .collect();
            let protected: Vec<&Path> = protected.iter().map(PathBuf::as_path).collect();
            // The one place a group's recorded trees are made ready: checked out in a lent
            // tree, else written out cold under the scratch folder.
            for (tree, lent) in recorded.trees.iter_mut().zip(lent) {
                if stop.is_stopped() {
                    stopped = true;
                    break;
                }
                if let Some((path, env)) = lent {
                    // Taken out with no await since the stop was asked: a drain that already
                    // cleared it stopped the run too.
                    let taken = {
                        let mut held = leases.borrow_mut();
                        held.iter()
                            .position(|lease| lease.path() == path)
                            .map(|index| held.remove(index))
                    };
                    let Some(lease) = taken else {
                        stopped = true;
                        break;
                    };
                    match check_out_tree_in_lent_tree(&git, &path, &tree.tree, &protected, &stop)
                        .await
                    {
                        Ok(()) if !stop.is_stopped() => {
                            leases.borrow_mut().push(lease);
                            tree.path = path;
                            tree.env = env;
                            continue;
                        }
                        Ok(()) | Err(LentTreeError::Stopped) => {
                            stopped = true;
                            break;
                        }
                        Err(LentTreeError::Failed(why)) => {
                            log::warn!(
                                "the closing checks of {id} run cold: the lent tree is not usable ({why})"
                            );
                            drop(lease);
                        }
                    }
                    if stop.is_stopped() {
                        stopped = true;
                        break;
                    }
                }
                tree.env = EnvBase::Own;
                match TempTree::materialize(
                    &git,
                    &tree.repository,
                    &scratch,
                    &tree.tree,
                    &index_dir,
                )
                .await
                {
                    Ok(temp) => {
                        tree.path = temp.path().to_path_buf();
                        temps.push(temp);
                    }
                    Err(why) => {
                        failure = Some(why);
                        break;
                    }
                }
            }
            if stopped || stop.is_stopped() {
                log::info!("the closing checks of {id} did not start: the engine is drained");
                stopped = true;
            }
            match failure {
                _ if stopped => {
                    this.update(cx, |engine, _| forget_started(engine, &entries))
                        .ok();
                }
                Some(why) => {
                    log::error!("running the closing checks of {id}: {why}");
                    this.update(cx, |engine, _| {
                        engine.decider.blocked.insert(id.clone(), why);
                    })
                    .ok();
                }
                None => {
                    let request = RunChecksRequest {
                        task_id: id.clone(),
                        checks: Some(names),
                        clean: false,
                        started_by: StartedBy::GroupDecision,
                        node: None,
                        params: None,
                    };
                    let started = this.update(cx, |engine, cx| {
                        engine.start_recorded(
                            request,
                            group,
                            recorded,
                            stop.clone(),
                            leases.clone(),
                            cx,
                        )
                    });
                    match started {
                        Ok(Ok(report)) => {
                            report.await;
                        }
                        Ok(Err(refusal)) => {
                            log::warn!("the closing checks of {id} did not start: {refusal}");
                            this.update(cx, |engine, _| forget_started(engine, &entries))
                                .ok();
                        }
                        Err(_) => {}
                    }
                }
            }
            leases.borrow_mut().clear();
            for temp in temps {
                temp.remove().await;
            }
            this.update(cx, |engine, _| {
                engine.decider.group_runs.remove(&id);
                engine.decider.group_holds.remove(&id);
                if let Some(followed) = engine.decider.followed_poke(&project) {
                    followed.unbounded_send(()).ok();
                }
            })
            .ok();
            drop(ended);
        })
        .detach();
    }
}

/// Deletes every ref of a kept tree, in each code repository of the project, whose task has no
/// done record and is no `checks.last` row's child any more, or whose groups with closing checks
/// all have their own done records.
pub(crate) async fn clean_refs(
    this: &WeakEntity<ChecksEngine>,
    writer: &Arc<StateWriter>,
    pinned: &Pinned,
    repo: &ProjectRepo,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    fn collect(node: &bongos_project_repo_model::DefinitionNode, names: &mut BTreeSet<String>) {
        if let Some(repository) = &node.definition.repository {
            names.insert(repository.clone());
        }
        names.extend(node.definition.repositories.iter().cloned());
        for child in &node.children {
            collect(child, names);
        }
    }
    if let Some(top) = &pinned.tree.top {
        collect(top, &mut names);
    }
    let snapshot = writer.read(snapshot)?;
    let directories: BTreeSet<PathBuf> = this.update(cx, |engine, cx| {
        // The repositories the tasks with records name, too, when the definitions name none.
        let tasks = snapshot
            .done
            .keys()
            .chain(snapshot.owed.keys())
            .chain(snapshot.last.values().map(|row| &row.id));
        for id in tasks {
            if let Some(name) = engine.task(id, cx).and_then(|task| task.repo) {
                names.insert(name);
            }
        }
        names
            .iter()
            .filter_map(|name| engine.source.code_repository(name, cx))
            .collect()
    })?;
    let backed: BTreeSet<&String> = snapshot
        .done
        .keys()
        .chain(snapshot.last.values().map(|row| &row.id))
        .collect();
    let git = TaskGit::of(repo).with_primaries(directories.iter().cloned());
    let git = &git;
    for directory in directories {
        let refs = match list_refs(git, &directory).await {
            Ok(refs) => refs,
            Err(error) => {
                log::warn!(
                    "listing the kept trees of {}: {error:#}",
                    directory.display()
                );
                continue;
            }
        };
        for (name, task) in refs {
            let ancestors_done = closing_ancestors(&task, pinned)
                .iter()
                .all(|group| snapshot.done.contains_key(group));
            if (!backed.contains(&task) || ancestors_done)
                && let Err(error) = delete_ref(git, &directory, &name).await
            {
                log::warn!("deleting {name}: {error:#}");
            }
        }
    }
    Ok(())
}
