//! The done decider: the half of the done rule that runs in the process holding a project's
//! state writer, for leaves. A task is held while it has an owed marker (`checks.owed`) and no
//! valid done record (`checks.done`); the decider judges each such leaf and, when every
//! required check has a fresh pass on the task's tree as it is now, writes the done record
//! `{ tree_key, generation, at, evidence }` in one transaction. The marker stays: the done
//! record alone clears the hold.
//!
//! What a leaf is judged on: its effective set in the definitions of the verified commit (read
//! from a pinned copy, so the digests are those of the checks as master has them), with the
//! closing checks of the groups its marker names. Each check's current state is its tree key,
//! key inputs and digest, computed now; satisfaction is [`satisfied`] over the task's evidence.
//! A leaf whose required set is now empty gets a done record with no evidence.
//!
//! A done record is written only in a transaction that finds, on its own reads, the same marker
//! (a task finished again has a new one), no done record, the same verified commit, and the
//! same satisfying evidence. Anything else writes nothing and leaves the task held; the change
//! that caused it brings the next decision. A tree key, a key input or a digest that cannot be
//! computed writes nothing either: the task stays held with that reason.
//!
//! When it decides: once when it starts following a project's writer (which also clears
//! `checks.live`, so every `hold/` row is written again from what is true now), after every
//! committed transaction that writes more than `checks.live` (a finish, a reopen, new
//! evidence, the landing that moves the verified commit), and on each change of the verified
//! commit. Decisions of one project run one after another. Groups are left alone here.
//!
//! The reasons in the `hold/` rows: "checking the tree" while the first decision runs, "not
//! done: N checks missing", "not done: no tree to check", or what could not be computed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use bongos_checks_model::evidence::{
    CheckState, Current, EvidenceRecord, RowStatus, Satisfaction, row_status, satisfied,
};
use bongos_checks_model::verdict::NoVerdictReason;
use bongos_checks_model::{Applicability, EffectiveSet, NamedSets, Row, effective_set};
use bongos_project_repo::{Git, ProjectRepo, StateEvent, StateHandle, StateRead, StateWriter};
use bongos_project_repo_model::{DefinitionNode, DefinitionTree};
use futures::channel::mpsc;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AsyncApp, Context, Task, WeakEntity};
use settings::Settings as _;

use crate::definitions::named_sets;
use crate::engine::{ChecksEngine, check_digest, check_repos, repo_trees, tip_trees_of};
use crate::evidence_store::{
    DONE_NAMESPACE, DoneRecord, LIVE_NAMESPACE, OWED_NAMESPACE, next_generation, put_done,
    scan_records, scan_records_in, task_prefix,
};
use crate::host::ChecksForwarderRegistry;
use crate::live::{HoldRow, hold_key};
use crate::owed::{OwedMarker, marker_in, uses_unknown_set};
use crate::pin::Pin;
use crate::settings::TaskChecksSettings;
use crate::task_source::TaskChecks;
use crate::task_tree::TaskTree;
use crate::tree_key::{TreeKey, combine_trees, key_inputs_digest, tree_key};
use crate::trees::{TaskTrees, TaskTreesRegistry};

/// The reason of a held task while its tree key is computed for the first time.
pub const CHECKING_TREE: &str = "checking the tree";
/// The reason of a held task that has no tree for its checks.
pub const NO_TREE: &str = "not done: no tree to check";
/// What `would_hold` answers for a group with closing checks and open children.
pub const OPEN_CHILDREN: &str = "complete or drop its open children first: the closing checks run on the tree of the last finished child";

/// "not done: N checks missing".
pub fn missing_reason(count: usize) -> String {
    format!("not done: {count} {} missing", checks_word(count))
}

/// "not done: N checks missing (a, b)", naming the required checks without a fresh pass in the
/// order of `rows`.
pub fn missing_reason_named(rows: &[Row], satisfaction: &Satisfaction) -> String {
    let names: Vec<&str> = rows
        .iter()
        .map(|row| row.decl.name.as_str())
        .filter(|name| {
            [
                &satisfaction.missing,
                &satisfaction.failing,
                &satisfaction.no_verdict,
                &satisfaction.stale,
            ]
            .iter()
            .any(|list| list.iter().any(|other| other == name))
        })
        .collect();
    format!(
        "{} ({})",
        missing_reason(satisfaction.unsatisfied_count()),
        names.join(", ")
    )
}

fn checks_word(count: usize) -> &'static str {
    if count == 1 { "check" } else { "checks" }
}

/// What the decider keeps: the projects it follows and the pinned definitions it judges by.
#[derive(Default)]
pub(crate) struct Decider {
    projects: BTreeMap<String, Followed>,
    pins: BTreeMap<String, Arc<Pinned>>,
    /// Projects whose last decision met a task the task source did not know yet; decided again
    /// when the Work tree changes.
    waiting: BTreeSet<String>,
    store_subscription: Option<gpui::Subscription>,
    #[cfg(test)]
    decided: Vec<mpsc::UnboundedSender<String>>,
}

/// One project whose writer the decider follows.
struct Followed {
    writer: Weak<StateWriter>,
    poke: mpsc::UnboundedSender<()>,
    /// The tasks whose `hold/` row this decider wrote.
    holds: BTreeSet<String>,
    _decisions: Task<()>,
}

impl Decider {
    /// Asks every followed project for a decision.
    pub(crate) fn poke_all(&self) {
        for followed in self.projects.values() {
            followed.poke.unbounded_send(()).ok();
        }
    }

    /// Forgets which `hold/` rows were written (a drain deleted them all).
    pub(crate) fn forget_holds(&mut self) {
        for followed in self.projects.values_mut() {
            followed.holds.clear();
        }
    }
}

/// The definitions of one commit, pinned, parsed, with their named sets.
pub(crate) struct Pinned {
    /// The commit pinned: the verified commit, or before one is recorded the commit shown.
    commit: String,
    /// The verified commit as the state store recorded it when the pin was taken.
    verified: Option<String>,
    pin: Pin,
    tree: DefinitionTree,
    sets: NamedSets,
}

/// How a leaf stands on its tree now.
pub(crate) enum Judgment {
    /// Every required check has a fresh pass on the tree whose key is `tree_key`.
    Pass {
        tree_key: String,
        evidence: Vec<String>,
        current: Current,
    },
    Unsatisfied(Satisfaction),
    NoTree,
    /// Something could not be computed; the words say what.
    Failed(String),
}

/// Whether a committed transaction calls for a decision: anything but writes to `checks.live`
/// alone (the decider's own `hold/` rows and the runs' part rows).
fn calls_for_decision(event: &StateEvent) -> bool {
    let StateEvent::Changed {
        task_ids,
        namespaces,
        ..
    } = event;
    !task_ids.is_empty()
        || namespaces.is_empty()
        || namespaces
            .iter()
            .any(|namespace| namespace != LIVE_NAMESPACE)
}

impl ChecksEngine {
    /// Whether this process decides now: the setting is on, the engine is not drained, and no
    /// forwarder sends its commands to another process that holds the writer.
    fn deciding(&self, cx: &App) -> bool {
        TaskChecksSettings::get_global(cx).enabled
            && !self.drained
            && ChecksForwarderRegistry::get(cx).is_none()
    }

    /// Starts following every open project whose state writer this process holds, while the
    /// setting is on; forgets the projects whose stores closed.
    pub fn scan_projects(&mut self, cx: &mut Context<Self>) {
        if !TaskChecksSettings::get_global(cx).enabled {
            return;
        }
        self.decider
            .projects
            .retain(|_, followed| followed.writer.strong_count() > 0);
        for loaded in bongos_work::OpenProjectRepos::global(cx).list() {
            self.follow_project(loaded.repo(), cx);
        }
        if self.decider.store_subscription.is_none()
            && let Some(store) = bongos_work::WorkStore::global(cx)
        {
            self.decider.store_subscription = Some(cx.subscribe(
                &store,
                |engine, _, event: &bongos_work::WorkStoreEvent, _| {
                    if matches!(event, bongos_work::WorkStoreEvent::Changed) {
                        engine.poke_waiting();
                    }
                },
            ));
        }
    }

    /// Stops deciding: every project is let go. The rows written stay; nothing reads them while
    /// the setting is off.
    pub fn stop_deciding(&mut self) {
        self.decider.projects.clear();
        self.decider.pins.clear();
        self.decider.waiting.clear();
        self.decider.store_subscription = None;
    }

    fn poke_waiting(&mut self) {
        for project in std::mem::take(&mut self.decider.waiting) {
            if let Some(followed) = self.decider.projects.get(&project) {
                followed.poke.unbounded_send(()).ok();
            }
        }
    }

    /// Follows the project of `repo` when this process holds its state writer: clears its
    /// `checks.live` (the store was just opened here, or a crash or handover left rows), decides
    /// once at once, and again after each transaction and each change of its verified commit.
    pub fn follow_project(&mut self, repo: &ProjectRepo, cx: &mut Context<Self>) {
        let Some(writer) = repo.state().and_then(StateHandle::writer).cloned() else {
            return;
        };
        let project = repo.location().id.clone();
        if self
            .decider
            .projects
            .get(&project)
            .is_some_and(|followed| std::ptr::eq(followed.writer.as_ptr(), Arc::as_ptr(&writer)))
        {
            return;
        }
        self.live_rows.adopt(&writer);
        let (poke, pokes) = mpsc::unbounded();
        poke.unbounded_send(()).ok();
        let states = writer
            .subscribe()
            .filter(|event| futures::future::ready(calls_for_decision(event)))
            .map(|_| ());
        let commits = repo.repo_events().map(|_| ());
        let weak = Arc::downgrade(&writer);
        let id = project.clone();
        let decisions = cx.spawn(async move |this, cx| {
            let mut triggers =
                futures::stream::select(pokes, futures::stream::select(states, commits));
            while triggers.next().await.is_some() {
                // Whatever else arrived meanwhile is covered by this one decision.
                while let Some(Some(())) = triggers.next().now_or_never() {}
                let Some(writer) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = decide_project(&this, &id, &writer, cx).await {
                    log::error!("deciding the checks of project {id}: {error:#}");
                }
                drop(writer);
                #[cfg(test)]
                this.update(cx, |engine, _| {
                    engine
                        .decider
                        .decided
                        .retain(|sender| sender.unbounded_send(id.clone()).is_ok());
                })
                .ok();
            }
        });
        self.decider.projects.insert(
            project,
            Followed {
                writer: Arc::downgrade(&writer),
                poke,
                holds: BTreeSet::new(),
                _decisions: decisions,
            },
        );
    }

    /// The ids of the projects each decision finished for, from now on; for tests.
    #[cfg(test)]
    pub(crate) fn decisions(&mut self) -> mpsc::UnboundedReceiver<String> {
        let (sender, receiver) = mpsc::unbounded();
        self.decider.decided.push(sender);
        receiver
    }

    /// Writes the `hold/` row of `task_id` (none: deletes it), throttled with its part rows.
    fn write_hold(
        &mut self,
        project: &str,
        writer: &Arc<StateWriter>,
        task_id: &str,
        hold: Option<HoldRow>,
        cx: &mut Context<Self>,
    ) {
        if self.drained {
            return;
        }
        if let Some(followed) = self.decider.projects.get_mut(project) {
            if hold.is_some() {
                followed.holds.insert(task_id.to_string());
            } else {
                followed.holds.remove(task_id);
            }
        }
        let interval =
            Duration::from_millis(u64::from(TaskChecksSettings::get_global(cx).live_write_ms));
        let flush = self
            .live_rows
            .update_hold(writer, task_id, hold, self.clock.now(), interval);
        self.schedule_live(task_id, flush, cx);
    }

    /// Deletes the `hold/` rows this decider wrote for tasks that are no longer held.
    fn drop_stale_holds(
        &mut self,
        project: &str,
        writer: &Arc<StateWriter>,
        held: &BTreeSet<String>,
        cx: &mut Context<Self>,
    ) {
        let stale: Vec<String> = self
            .decider
            .projects
            .get(project)
            .map(|followed| followed.holds.difference(held).cloned().collect())
            .unwrap_or_default();
        for task_id in stale {
            self.write_hold(project, writer, &task_id, None, cx);
        }
    }

    /// Why `task_id` would be held if it were finished now; `None` when it would not be. Judged
    /// like a decision, on the task's tree as it is now, with the closing checks it would carry
    /// as the last open child; a group with closing checks and open children is refused. Nothing
    /// is written. What cannot be computed holds.
    pub fn would_hold(&mut self, task_id: &str, cx: &mut Context<Self>) -> Task<Option<String>> {
        if !TaskChecksSettings::get_global(cx).enabled {
            return Task::ready(None);
        }
        let Some(task) = self.task(task_id, cx) else {
            return Task::ready(None);
        };
        let Some(writer) = task.writer() else {
            // No state store: nothing can mark the task, so nothing holds it.
            return Task::ready(None);
        };
        let tree = self.task_tree(&task, cx);
        let trees = TaskTreesRegistry::get(cx);
        let done = self.source.done_tasks(cx);
        let task_id = task_id.to_string();
        cx.spawn(async move |this, cx| {
            let pinned = match pinned(&this, &task.project, cx).await {
                Ok(pinned) => pinned,
                Err(error) => {
                    return Some(format!(
                        "not done: the checks could not be read ({error:#})"
                    ));
                }
            };
            let set = effective_set(&task_id, &pinned.tree, &pinned.sets, &|id| {
                id != task_id && done.contains(id)
            });
            if matches!(
                set.applicability,
                Applicability::NoChecksApply | Applicability::UnknownTask
            ) {
                return None;
            }
            if uses_unknown_set(&task_id, &pinned.tree, &pinned.sets) {
                return Some("not done: a check set it uses could not be read".to_string());
            }
            if set.is_group {
                if set.required().next().is_none() {
                    return None;
                }
                let open = pinned.tree.node(&task_id).is_some_and(|node| {
                    node.children
                        .iter()
                        .any(|child| !done.contains(&child.definition.id))
                });
                return open.then(|| OPEN_CHILDREN.to_string());
            }
            let rows = set.judged().to_vec();
            if !rows.iter().any(|row| row.decl.is_required()) {
                return None;
            }
            match judge(
                &task,
                tree.as_ref(),
                trees.as_ref(),
                &rows,
                &pinned.pin,
                &writer,
            )
            .await
            {
                Judgment::Pass { .. } => None,
                Judgment::Unsatisfied(satisfaction) => {
                    Some(missing_reason_named(&rows, &satisfaction))
                }
                Judgment::NoTree => Some(NO_TREE.to_string()),
                Judgment::Failed(why) => Some(format!("not done: {why}")),
            }
        })
    }
}

/// Starts the decider of this app: it follows the projects whose writers this process holds
/// while `task_checks.enabled` is on, and lets them go while it is off.
pub(crate) fn init(engine: gpui::Entity<ChecksEngine>, cx: &mut App) {
    let sync = move |cx: &mut App| {
        let enabled = TaskChecksSettings::get_global(cx).enabled;
        engine.update(cx, |engine, cx| {
            if enabled {
                engine.scan_projects(cx);
            } else {
                engine.stop_deciding();
            }
        });
    };
    sync(cx);
    cx.observe_global::<settings::SettingsStore>(sync).detach();
}

/// The marked tasks of a store without a valid done record, with their markers. A marker that
/// cannot be read is logged and left alone: the task stays held.
fn held_tasks(read: &StateRead) -> Result<Vec<(String, OwedMarker)>> {
    let owed = read.kv(OWED_NAMESPACE).scan("")?;
    if owed.is_empty() {
        return Ok(Vec::new());
    }
    let done: BTreeMap<String, toml::Value> =
        read.kv(DONE_NAMESPACE).scan("")?.into_iter().collect();
    let mut held = Vec::new();
    for (task_id, value) in owed {
        if done
            .get(&task_id)
            .is_some_and(|value| value.clone().try_into::<DoneRecord>().is_ok())
        {
            continue;
        }
        match value.try_into::<OwedMarker>() {
            Ok(marker) => held.push((task_id, marker)),
            Err(error) => log::error!("reading the owed marker of {task_id}: {error}"),
        }
    }
    Ok(held)
}

/// One decision for every held task of the project `project` whose writer is `writer`.
async fn decide_project(
    this: &WeakEntity<ChecksEngine>,
    project: &str,
    writer: &Arc<StateWriter>,
    cx: &mut AsyncApp,
) -> Result<()> {
    if !this.update(cx, |engine, cx| engine.deciding(cx))? {
        return Ok(());
    }
    let held = writer.read(held_tasks)?;
    let ids: BTreeSet<String> = held.iter().map(|(id, _)| id.clone()).collect();
    this.update(cx, |engine, cx| {
        engine.drop_stale_holds(project, writer, &ids, cx)
    })?;
    if held.is_empty() {
        return Ok(());
    }
    // The project's repository, from any task the task source knows.
    let repo = this.update(cx, |engine, cx| {
        held.iter().find_map(|(id, _)| {
            engine
                .task(id, cx)
                .map(|task| task.project)
                .filter(|repo| repo.location().id == project)
        })
    })?;
    let Some(repo) = repo else {
        this.update(cx, |engine, _| {
            engine.decider.waiting.insert(project.to_string());
        })?;
        return Ok(());
    };
    let pinned = pinned(this, &repo, cx).await?;
    for (task_id, marker) in held {
        decide_task(this, project, writer, &pinned, &task_id, &marker, cx).await?;
    }
    Ok(())
}

/// The closing groups of a leaf: none from the top down until the first group its marker
/// names, then every group below that one, so the leaf carries the closing set of each group
/// it carried for when it finished (a group's closing set holds those passed down to it).
fn leaf_set(
    task_id: &str,
    tree: &DefinitionTree,
    sets: &NamedSets,
    closing_from: &[String],
) -> EffectiveSet {
    let mut chain: Vec<&DefinitionNode> = Vec::new();
    if let Some(top) = &tree.top {
        find_chain(top, task_id, &mut chain);
    }
    let mut done: HashSet<&str> = HashSet::new();
    let mut carrying = false;
    for pair in chain.windows(2) {
        let (group, child) = (pair[0], pair[1]);
        carrying |= closing_from.contains(&group.definition.id);
        if carrying {
            for sibling in &group.children {
                if sibling.definition.id != child.definition.id {
                    done.insert(sibling.definition.id.as_str());
                }
            }
        }
    }
    effective_set(task_id, tree, sets, &|id| done.contains(id))
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

/// Decides one held task: a leaf whose checks all pass on its tree now gets its done record;
/// any other leaf gets the reason it stays held. Groups and tasks the pinned definitions lack
/// are left alone.
async fn decide_task(
    this: &WeakEntity<ChecksEngine>,
    project: &str,
    writer: &Arc<StateWriter>,
    pinned: &Pinned,
    task_id: &str,
    marker: &OwedMarker,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some(node) = pinned.tree.node(task_id) else {
        log::warn!(
            "{task_id} owes checks but is not in the definitions of {}",
            pinned.commit
        );
        return Ok(());
    };
    if node.folder.is_empty() || !node.children.is_empty() {
        return Ok(());
    }
    let set = leaf_set(task_id, &pinned.tree, &pinned.sets, &marker.closing_from);
    let rows = set.judged().to_vec();
    let now = this.update(cx, |engine, _| engine.now.clone())?;
    if uses_unknown_set(task_id, &pinned.tree, &pinned.sets) {
        // A set that cannot be read would look like no checks at all.
        let reason = "not done: a check set it uses could not be read";
        this.update(cx, |engine, cx| {
            engine.write_hold(project, writer, task_id, Some(HoldRow::new(reason)), cx)
        })?;
        return Ok(());
    }
    if !rows.iter().any(|row| row.decl.is_required()) {
        let decision = Decision {
            task_id,
            marker,
            verified: &pinned.verified,
            rows: &rows,
            current: &Current::default(),
            evidence: &[],
            tree_key: "",
            at: now(),
        };
        if still_deciding(this, cx)? && write_done(writer, &decision)? {
            this.update(cx, |engine, cx| engine.done_written(project, task_id, cx))?;
        }
        return Ok(());
    }
    let context = this.update(cx, |engine, cx| {
        let task = engine
            .task(task_id, cx)
            .filter(|task| task.project.location().id == project)?;
        let tree = engine.task_tree(&task, cx);
        let first = !matches!(engine.live_rows.hold(task_id), Some(Some(_)));
        if first {
            engine.write_hold(
                project,
                writer,
                task_id,
                Some(HoldRow::new(CHECKING_TREE)),
                cx,
            );
        }
        Some((task, tree, TaskTreesRegistry::get(cx)))
    })?;
    let Some((task, tree, trees)) = context else {
        this.update(cx, |engine, _| {
            engine.decider.waiting.insert(project.to_string());
        })?;
        return Ok(());
    };
    let judgment = judge(
        &task,
        tree.as_ref(),
        trees.as_ref(),
        &rows,
        &pinned.pin,
        writer,
    )
    .await;
    let hold = match judgment {
        Judgment::Pass {
            tree_key,
            evidence,
            current,
        } => {
            let decision = Decision {
                task_id,
                marker,
                verified: &pinned.verified,
                rows: &rows,
                current: &current,
                evidence: &evidence,
                tree_key: &tree_key,
                at: now(),
            };
            if still_deciding(this, cx)? && write_done(writer, &decision)? {
                this.update(cx, |engine, cx| engine.done_written(project, task_id, cx))?;
            }
            return Ok(());
        }
        Judgment::Unsatisfied(satisfaction) => {
            let count = satisfaction.unsatisfied_count();
            HoldRow {
                reason: missing_reason(count),
                unsatisfied: Some(u32::try_from(count).unwrap_or(u32::MAX)),
            }
        }
        Judgment::NoTree => HoldRow::new(NO_TREE),
        Judgment::Failed(why) => HoldRow::new(format!("not done: {why}")),
    };
    this.update(cx, |engine, cx| {
        engine.write_hold(project, writer, task_id, Some(hold), cx)
    })?;
    Ok(())
}

impl ChecksEngine {
    /// A done record of `task_id` was written, its `hold/` row deleted with it.
    fn done_written(&mut self, project: &str, task_id: &str, cx: &mut Context<Self>) {
        self.live_rows.hold_deleted(task_id);
        if let Some(followed) = self.decider.projects.get_mut(project) {
            followed.holds.remove(task_id);
        }
        cx.emit(crate::engine::ChecksEvent::Changed {
            task_id: task_id.to_string(),
        });
    }
}

/// Whether the decider may still write: a drain or the setting going off while a decision ran
/// stops it before its done record.
fn still_deciding(this: &WeakEntity<ChecksEngine>, cx: &mut AsyncApp) -> Result<bool> {
    this.update(cx, |engine, cx| engine.deciding(cx))
}

/// The pinned definitions of `repo`'s verified commit (or, before one is recorded, of the
/// commit shown), reused while that commit stays.
async fn pinned(
    this: &WeakEntity<ChecksEngine>,
    repo: &ProjectRepo,
    cx: &mut AsyncApp,
) -> Result<Arc<Pinned>> {
    let project = repo.location().id.clone();
    let verified = repo.verified_commit()?.map(|commit| commit.0);
    let commit = match &verified {
        Some(commit) => commit.clone(),
        None => repo.shown_commit().await?.0,
    };
    let cached = this.update(cx, |engine, _| {
        engine
            .decider
            .pins
            .get(&project)
            .filter(|pinned| pinned.commit == commit && pinned.verified == verified)
            .cloned()
    })?;
    if let Some(pinned) = cached {
        return Ok(pinned);
    }
    let pin = Pin::materialize(repo, &commit).await?;
    let definitions = pin.definitions();
    let tree = DefinitionTree::parse_dir(&definitions).map_err(|problems| {
        anyhow!(
            "the definitions of {commit} do not parse ({} problems)",
            problems.len()
        )
    })?;
    let sets = named_sets(&definitions);
    let pinned = Arc::new(Pinned {
        commit,
        verified,
        pin,
        tree,
        sets,
    });
    this.update(cx, |engine, _| {
        engine.decider.pins.insert(project, pinned.clone());
    })?;
    Ok(pinned)
}

/// The current state of every row of `rows` on `task`'s trees, the key of the trees its
/// required rows run in, and how the rows stand against the task's evidence.
pub(crate) async fn judge(
    task: &TaskChecks,
    tree: Option<&TaskTree>,
    trees: Option<&Rc<dyn TaskTrees>>,
    rows: &[Row],
    pin: &Pin,
    writer: &StateWriter,
) -> Judgment {
    let Some(tree) = tree else {
        return Judgment::NoTree;
    };
    let mut keys = TreeKeys::default();
    let mut current = Current::default();
    let mut required_repos: Vec<String> = Vec::new();
    for row in rows {
        let required = row.decl.is_required();
        if required {
            for repo in check_repos(task, &row.decl) {
                if !required_repos.contains(&repo) {
                    required_repos.push(repo);
                }
            }
        }
        match state_of(task, &tree.path, trees, row, pin, &mut keys).await {
            Ok(state) => {
                current.checks.insert(row.decl.name.clone(), state);
            }
            // An advisory check that cannot be judged only accepts nothing.
            Err(_) if !required => {}
            Err(judgment) => return judgment,
        }
    }
    let Ok(done_trees) = repo_trees(task, &tree.path, trees, required_repos) else {
        return Judgment::NoTree;
    };
    let tree_key = match keys
        .key(task.project.git(), &task.index_dir, &done_trees)
        .await
    {
        Ok(key) => key,
        Err(judgment) => return judgment,
    };
    let records = match writer.read(|read| scan_records(read, &task_prefix(&task.task_id))) {
        Ok(records) => records,
        Err(error) => {
            return Judgment::Failed(format!("the evidence could not be read ({error:#})"));
        }
    };
    let satisfaction = satisfied(rows, &records, &current, &task.task_id);
    if !satisfaction.is_satisfied() {
        return Judgment::Unsatisfied(satisfaction);
    }
    Judgment::Pass {
        tree_key,
        evidence: evidence_of(rows, &records, &current, &task.task_id),
        current,
    }
}

/// The key of each repository's tree, computed once per judgment. Every key of the judgment
/// (each check's and the done record's over the union of the required repositories) is derived
/// from these same per-repository keys, so an edit landing in one tree while the judgment runs
/// cannot give the done record a key some check never passed on.
#[derive(Default)]
pub(crate) struct TreeKeys(BTreeMap<String, TreeKey>);

impl TreeKeys {
    /// The key of `trees` (as [`crate::tree_key::trees_key`] derives it), from the key each
    /// repository's tree had the first time this judgment asked for it.
    pub(crate) async fn key(
        &mut self,
        git: &Git,
        index_dir: &Path,
        trees: &[crate::engine::RepoTree],
    ) -> Result<String, Judgment> {
        let mut keys: Vec<(&str, TreeKey)> = Vec::with_capacity(trees.len());
        for tree in trees {
            let key = match self.0.get(&tree.repo) {
                Some(key) => key.clone(),
                None => {
                    let key = tree_key(git, &tree.path, index_dir)
                        .await
                        .map_err(|error| {
                            Judgment::Failed(format!(
                                "the tree key could not be computed ({})",
                                NoVerdictReason::TreeUnavailable(error.to_string()).text()
                            ))
                        })?;
                    self.0.insert(tree.repo.clone(), key.clone());
                    key
                }
            };
            keys.push((tree.repo.as_str(), key));
        }
        Ok(match keys.as_slice() {
            [(_, key)] => key.as_str().to_string(),
            _ => combine_trees(&keys).as_str().to_string(),
        })
    }
}

/// What `row`'s check is now on `task`'s trees: their key, its key inputs and its digest.
async fn state_of(
    task: &TaskChecks,
    own_tree: &Path,
    trees: Option<&Rc<dyn TaskTrees>>,
    row: &Row,
    pin: &Pin,
    keys: &mut TreeKeys,
) -> Result<CheckState, Judgment> {
    let tips = tip_trees_of(task, own_tree, trees, &row.decl).map_err(|_| Judgment::NoTree)?;
    let Some(first) = tips.first() else {
        return Err(Judgment::NoTree);
    };
    let inputs: Vec<PathBuf> = row
        .decl
        .key_inputs
        .iter()
        .map(|path| first.path.join(path))
        .collect();
    let tree_key = keys.key(task.project.git(), &task.index_dir, &tips).await?;
    let key_inputs = key_inputs_digest(&inputs).map_err(|error| {
        Judgment::Failed(format!(
            "the key inputs of {} could not be read ({error})",
            row.decl.name
        ))
    })?;
    let digest = check_digest(&row.decl, pin).map_err(|reason| {
        Judgment::Failed(format!(
            "{} could not be read ({})",
            row.decl.name,
            reason.text()
        ))
    })?;
    Ok(CheckState::new(&tree_key, &key_inputs, &digest))
}

/// The ids of the records that satisfy the required rows, each once, in the order of `rows`.
fn evidence_of(
    rows: &[Row],
    records: &[EvidenceRecord],
    current: &Current,
    task_id: &str,
) -> Vec<String> {
    let mut evidence: Vec<String> = Vec::new();
    for row in rows.iter().filter(|row| row.decl.is_required()) {
        if let RowStatus::Satisfied { record_id, .. } =
            row_status(row, records, current, rows, task_id)
            && !evidence.contains(&record_id)
        {
            evidence.push(record_id);
        }
    }
    evidence
}

/// A done record about to be written, with what it was decided on.
pub(crate) struct Decision<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) marker: &'a OwedMarker,
    pub(crate) verified: &'a Option<String>,
    pub(crate) rows: &'a [Row],
    pub(crate) current: &'a Current,
    pub(crate) evidence: &'a [String],
    pub(crate) tree_key: &'a str,
    pub(crate) at: String,
}

/// Writes the done record of `decision` in one transaction, if that transaction still finds the
/// marker decided on, no done record, the verified commit decided on, and, on its own read of
/// the evidence, every required row satisfied by the same records. Returns whether it wrote.
/// The task's `hold/` row goes in the same transaction.
pub(crate) fn write_done(writer: &StateWriter, decision: &Decision) -> Result<bool> {
    writer
        .transact(|tx| {
            if marker_in(tx, decision.task_id)?.as_ref() != Some(decision.marker) {
                return Ok(false);
            }
            if tx.kv(DONE_NAMESPACE).get(decision.task_id)?.is_some() {
                return Ok(false);
            }
            if tx.verified_commit()? != *decision.verified {
                return Ok(false);
            }
            let records = scan_records_in(tx, &task_prefix(decision.task_id))?;
            let satisfaction =
                satisfied(decision.rows, &records, decision.current, decision.task_id);
            if !satisfaction.is_satisfied()
                || evidence_of(decision.rows, &records, decision.current, decision.task_id)
                    != decision.evidence
            {
                return Ok(false);
            }
            let generation = next_generation(tx)?;
            put_done(
                tx,
                decision.task_id,
                &DoneRecord {
                    tree_key: decision.tree_key.to_string(),
                    generation,
                    at: decision.at.clone(),
                    evidence: decision.evidence.to_vec(),
                },
            )?;
            let hold = hold_key(decision.task_id);
            if tx.kv(LIVE_NAMESPACE).get(&hold)?.is_some() {
                tx.kv(LIVE_NAMESPACE).delete(&hold)?;
            }
            Ok(true)
        })
        .context("writing a done record")
}

#[cfg(test)]
impl ChecksEngine {
    /// One decision for the project `project` whose writer is `writer`, outside any followed
    /// loop; for tests.
    pub(crate) fn decide_once(
        &mut self,
        project: &str,
        writer: Arc<StateWriter>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let project = project.to_string();
        cx.spawn(async move |this, cx| {
            decide_project(&this, &project, &writer, cx)
                .await
                .expect("the decision runs");
        })
    }
}
