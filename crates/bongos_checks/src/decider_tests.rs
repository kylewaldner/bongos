//! Tests of the done decider: a leaf whose checks pass on its tree as it is now gets its done
//! record, any other stays held with its reason, and a decision taken for an earlier finish never
//! lands on a later one. Checks run through the engine, so the decider judges real records with
//! real tree keys; time is a counter, never the wall clock.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bongos_checks_model::verdict::Verdict;
use bongos_project_repo::{
    Git, LandingResult, ProjectRepo, StateApplyHooks, StateHandle, StateWriter,
};
use bongos_project_repo_model::{DefinitionTree, TaskStateRecord};
use bongos_work::{DoneHoldSource as _, NeedsYouRegistry, RowChipsRegistry};
use bongos_work_model::Status;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::decider::{CHECKING_TREE, Decision, NO_TREE, TreeKeys, write_done};
use crate::engine::{ChecksEngine, RepoTree, RunChecksRequest};
use crate::evidence_store::{DoneRecord, done_record, scan_records, task_prefix};
use crate::holds::{ChecksChips, ChecksHoldSource, ChecksHolds};
use crate::live::{HoldRow, hold_key};
use crate::owed::{ChecksOwedHook, FolderSets, OwedMarker};
use crate::pin::master_definitions;
use crate::pools_tests::FakeClock;
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource, effective_set_in};
use crate::task_tree::RunWorktrees;
use crate::test_support::{
    commit_everything, definition_toml, fixture_project_with_hooks, init_git_repo, land,
    main_commit, new_copy, person, write_definition, write_file,
};
use crate::trees::EnvBase;
use crate::{LIVE_NAMESPACE, OWED_NAMESPACE, read_hold_rows};

const TASK: &str = "l1";
const PROJECT: &str = "project-1";
const TESTS: &str = "\n[[checks.check]]\nname = \"tests\"\ncommand = [\"true\"]\n";

#[derive(Default)]
struct Source {
    tasks: RefCell<BTreeMap<String, TaskChecks>>,
}

impl TaskSource for Source {
    fn task(&self, task_id: &str, _: &gpui::App) -> Option<TaskChecks> {
        self.tasks.borrow().get(task_id).cloned()
    }

    fn done_tasks(&self, _: &gpui::App) -> HashSet<String> {
        HashSet::new()
    }
}

struct NoRuns;

impl RunWorktrees for NoRuns {
    fn active(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }

    fn last_ended(&self, _: &str, _: &gpui::App) -> Option<(String, PathBuf)> {
        None
    }
}

/// What the task's file names as its worktree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tree {
    /// A git tree.
    Git,
    /// No worktree at all.
    None,
    /// A folder that is not a git tree, whose key cannot be computed.
    NotGit,
}

/// Fields drop in order: the engine and the project go before the folder holding them.
struct World {
    engine: Entity<ChecksEngine>,
    source: Rc<Source>,
    repo: ProjectRepo,
    tree: PathBuf,
    _root: TempDir,
}

impl World {
    /// The project `project-1` with the leaf `l1` declaring `checks`, its verified commit
    /// recorded, the owed hook on its writer, and the engine with the setting on. `l1`'s tree is
    /// as `tree` says.
    fn new(cx: &mut TestAppContext, checks: &str, tree: Tree) -> Self {
        let root = TempDir::new().unwrap();
        let repository = root.path().join("project/repository");
        let hooks = StateApplyHooks::new();
        let hook = Arc::new(ChecksOwedHook::new());
        hook.set_enabled(true);
        hook.set_named_sets(Arc::new(FolderSets(repository.join("definitions"))));
        hooks.register(hook);
        let repo = fixture_project_with_hooks(&root, &hooks);
        std::fs::write(
            repository.join("definitions/tasks/backfill/task.toml"),
            definition_toml(TASK, "Backfill", checks),
        )
        .unwrap();
        commit_everything(&repository, "add checks");
        let world_tree = root.path().join("tree");
        std::fs::create_dir_all(&world_tree).unwrap();
        if tree == Tree::Git {
            init_git_repo(&world_tree);
            write_file(&world_tree, "src/lib.rs", "pub fn one() {}\n");
            commit_everything(&world_tree, "start");
        }
        let source = Rc::new(Source::default());
        let counter = Arc::new(AtomicU64::new(0));
        let engine = cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            TaskChecksSettings::register(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    let checks = content.task_checks.get_or_insert_default();
                    checks.enabled = Some(true);
                    checks.live_write_ms = Some(0);
                });
            });
            ChecksEngine::install(
                ChecksEngine::new(
                    source.clone(),
                    Rc::new(NoRuns),
                    FakeClock::automatic(),
                    Arc::new(move || {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        format!("2026-03-01T10:{:02}:{:02}Z", n / 60 % 60, n % 60)
                    }),
                ),
                cx,
            )
        });
        let world = Self {
            engine,
            source,
            repo,
            tree: world_tree,
            _root: root,
        };
        world.record_verified();
        let worktree = match tree {
            Tree::Git | Tree::NotGit => Some(world.tree.clone()),
            Tree::None => None,
        };
        let task = world.task(worktree);
        world
            .source
            .tasks
            .borrow_mut()
            .insert(TASK.to_string(), task);
        world
    }

    fn task(&self, worktree: Option<PathBuf>) -> TaskChecks {
        let root = self._root.path();
        TaskChecks {
            task_id: TASK.to_string(),
            name: "Backfill".to_string(),
            repo: Some("app".to_string()),
            worktree,
            set: effective_set_in(&master_definitions(&self.repo), TASK, &|_| false)
                .expect("the definitions parse"),
            project: self.repo.clone(),
            base_branch: Some("main".to_string()),
            output_dir: root.join("output"),
            index_dir: root.join("index"),
        }
    }

    /// Records master's commit as verified, with its definitions, as a landing does.
    fn record_verified(&self) {
        let commit = main_commit(&self.repo);
        let definitions = Arc::new(
            DefinitionTree::parse_dir(&master_definitions(&self.repo)).expect("they parse"),
        );
        self.writer()
            .transact(|tx| tx.set_verified_commit(&commit, definitions.clone()))
            .unwrap();
    }

    fn writer(&self) -> Arc<StateWriter> {
        self.repo.state().unwrap().writer().unwrap().clone()
    }

    fn set_status(&self, status: Status) {
        self.writer()
            .transact(|tx| {
                tx.set_task(
                    TASK,
                    TaskStateRecord {
                        status,
                        ..TaskStateRecord::default()
                    },
                )
            })
            .unwrap();
    }

    fn finish(&self) {
        self.set_status(Status::Done);
    }

    fn reopen(&self) {
        self.set_status(Status::Open);
    }

    fn edit(&self, text: &str) {
        write_file(&self.tree, "src/lib.rs", text);
    }

    async fn run_checks(&self, cx: &mut TestAppContext) {
        let report = self
            .engine
            .update(cx, |engine, cx| {
                engine.run(RunChecksRequest::person(TASK), cx)
            })
            .await;
        assert!(
            report
                .results()
                .iter()
                .all(|result| result.verdict == Verdict::Pass),
            "{report:?}"
        );
    }

    async fn decide(&self, cx: &mut TestAppContext) {
        let writer = self.writer();
        self.engine
            .update(cx, |engine, cx| engine.decide_once(PROJECT, writer, cx))
            .await;
    }

    fn done(&self) -> Option<DoneRecord> {
        self.writer().read(|read| done_record(read, TASK)).unwrap()
    }

    fn marker(&self) -> Option<OwedMarker> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(OWED_NAMESPACE)
                    .get(TASK)?
                    .map(|value| value.try_into().unwrap()))
            })
            .unwrap()
    }

    /// The task's hold as every process reads it: `None` when it is not held.
    fn hold(&self) -> Option<HoldRow> {
        self.writer().read(read_hold_rows).unwrap().remove(TASK)
    }

    fn hold_row_exists(&self) -> bool {
        self.writer()
            .read(|read| Ok(read.kv(LIVE_NAMESPACE).get(&hold_key(TASK))?.is_some()))
            .unwrap()
    }

    /// The ids of the task's records, oldest first.
    fn record_ids(&self) -> Vec<String> {
        let mut records = self
            .writer()
            .read(|read| scan_records(read, &task_prefix(TASK)))
            .unwrap();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        records.into_iter().map(|record| record.id).collect()
    }

    fn records_tree_key(&self) -> String {
        let records = self
            .writer()
            .read(|read| scan_records(read, &task_prefix(TASK)))
            .unwrap();
        records
            .iter()
            .max_by(|a, b| a.id.cmp(&b.id))
            .map(|record| record.tree_key.clone())
            .expect("a record")
    }

    async fn would_hold(&self, cx: &mut TestAppContext) -> Option<String> {
        cx.update(|cx| crate::host::would_hold(TASK, cx)).await
    }
}

fn missing(count: u32) -> HoldRow {
    HoldRow {
        reason: crate::decider::missing_reason(count as usize),
        unsatisfied: Some(count),
    }
}

#[gpui::test]
async fn a_pass_on_the_current_tree_makes_the_task_done_and_writes_the_record(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    world.run_checks(cx).await;
    world.finish();
    assert!(world.marker().is_some(), "finishing marks the task");
    assert!(world.hold().is_some(), "held until the decider judged it");

    world.decide(cx).await;

    let done = world.done().expect("a done record");
    assert_eq!(done.tree_key, world.records_tree_key());
    assert_eq!(done.evidence, world.record_ids());
    assert!(done.generation > 0);
    assert_eq!(world.hold(), None, "Done: no hold");
    assert!(!world.hold_row_exists());
    // The marker stays: the done record alone clears the hold.
    assert!(world.marker().is_some());
    // Judged again, a valid done record is left as it is.
    world.decide(cx).await;
    assert_eq!(world.done(), Some(done));
}

#[gpui::test]
async fn an_edit_after_the_pass_keeps_the_task_held_until_the_tree_is_back(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    world.run_checks(cx).await;
    world.edit("pub fn two() {}\n");
    world.finish();

    world.decide(cx).await;

    assert_eq!(world.done(), None, "the pass is of an earlier tree");
    assert_eq!(world.hold(), Some(missing(1)));
    assert_eq!(
        world.would_hold(cx).await.as_deref(),
        Some("not done: 1 check missing (tests)")
    );

    // Reverting the edit gives the old key back, so the old pass is fresh again.
    world.edit("pub fn one() {}\n");
    world.decide(cx).await;
    assert!(world.done().is_some());
    assert_eq!(world.hold(), None);
}

#[gpui::test]
async fn a_task_with_no_tree_stays_held(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::None);
    world.finish();

    world.decide(cx).await;

    assert_eq!(world.done(), None);
    assert_eq!(world.hold(), Some(HoldRow::new(NO_TREE)));
    assert_eq!(world.would_hold(cx).await.as_deref(), Some(NO_TREE));
}

#[gpui::test]
async fn a_tree_key_that_cannot_be_computed_leaves_the_task_held_with_its_reason(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::NotGit);
    world.finish();

    world.decide(cx).await;

    assert_eq!(world.done(), None);
    let hold = world.hold().expect("still held");
    assert!(
        hold.reason
            .starts_with("not done: the tree key could not be computed"),
        "{hold:?}"
    );
    assert_ne!(hold.reason, CHECKING_TREE);
}

#[gpui::test]
async fn a_reopened_and_closed_again_task_is_judged_normally(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    world.run_checks(cx).await;
    world.finish();
    world.decide(cx).await;
    let first = world.done().expect("Done after the first finish");
    let first_marker = world.marker().expect("marked");

    // Reopening deletes the marker and the done record; the tree changes; it finishes again.
    world.reopen();
    assert_eq!(world.marker(), None);
    assert_eq!(world.done(), None);
    world.edit("pub fn two() {}\n");
    world.finish();
    let second_marker = world.marker().expect("marked again");
    assert_ne!(second_marker, first_marker, "a new finish has a new marker");

    // A decision taken for the first finish does not land on the second.
    let verified = Some(main_commit(&world.repo));
    let wrote = write_done(
        &world.writer(),
        &Decision {
            task_id: TASK,
            marker: &first_marker,
            verified: &verified,
            rows: &[],
            current: &Default::default(),
            evidence: &[],
            tree_key: &first.tree_key,
            at: "2026-03-01T10:00:00Z".to_string(),
        },
    )
    .unwrap();
    assert!(!wrote);
    assert_eq!(world.done(), None);

    world.decide(cx).await;
    assert_eq!(
        world.done(),
        None,
        "the pass is of the tree before the edit"
    );
    assert_eq!(world.hold(), Some(missing(1)));

    world.run_checks(cx).await;
    world.decide(cx).await;
    let second = world.done().expect("Done after a pass on the new tree");
    assert_ne!(second.tree_key, first.tree_key);
    assert!(second.generation > first.generation);
}

#[gpui::test]
async fn removing_a_held_tasks_check_then_adding_it_back_leaves_it_done(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    let mut decisions = world.engine.update(cx, |engine, cx| {
        let decisions = engine.decisions();
        engine.follow_project(&world.repo, cx);
        decisions
    });
    // Every change of state is followed by a decision; wait for the one that settles `done`.
    async fn until(
        decisions: &mut mpsc::UnboundedReceiver<String>,
        world: &World,
        settled: impl Fn(&World) -> bool,
    ) {
        while !settled(world) {
            decisions.next().await.expect("the decider runs");
        }
    }
    world.finish();
    until(&mut decisions, &world, |world| {
        world.hold() == Some(missing(1))
    })
    .await;
    assert_eq!(world.done(), None);

    // The person at the keyboard lands the change that removes the check.
    let author = person();
    let copy = new_copy(&world.repo, "amber-otter-0001", &author);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &definition_toml(TASK, "Backfill", ""),
    );
    let landed = land(&world.repo, &copy, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");

    // The landing's change of the verified commit brings the decision: nothing is required
    // now, so the done record has no evidence.
    until(&mut decisions, &world, |world| world.done().is_some()).await;
    let done = world.done().unwrap();
    assert!(done.evidence.is_empty());
    assert_eq!(world.hold(), None);
    assert!(world.marker().is_some(), "the marker stays");

    // Adding the check back does not mark the task again: it stays Done.
    let copy = new_copy(&world.repo, "bold-crane-0002", &author);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &definition_toml(TASK, "Backfill", TESTS),
    );
    let landed = land(&world.repo, &copy, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
    decisions.next().await.expect("the decider runs");
    assert_eq!(world.done(), Some(done));
    assert_eq!(world.hold(), None);
}

#[gpui::test]
async fn the_chip_counts_the_missing_checks_and_is_not_needs_you(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    world.finish();
    world.decide(cx).await;
    assert_eq!(world.hold(), Some(missing(1)));

    let handle = StateHandle::Writer(world.writer());
    cx.update(|cx| {
        crate::holds::init(cx);
        let holds = ChecksHolds::global(cx).unwrap();
        holds.refresh_from(&[(PROJECT.to_string(), handle.clone())]);
        let chips = bongos_work::RowChipsSource::chips(&ChecksChips(holds), TASK, cx);
        assert_eq!(chips, ["checks 1"]);
        // Registered with the row chips while the setting is on, and never as "needs you".
        assert_eq!(RowChipsRegistry::chips(TASK, cx), ["checks 1"]);
        assert!(NeedsYouRegistry::collect(cx).is_empty());
    });
}

#[gpui::test]
async fn would_hold_asks_the_decider_through_checks_access(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, TESTS, Tree::Git);
    let holds = Arc::new(ChecksHolds::new(
        bongos_work::OpenProjectRepos::default(),
        mpsc::unbounded().0,
    ));
    let source = ChecksHoldSource(holds);

    let before = cx.update(|cx| source.would_hold(TASK, cx)).await;
    assert_eq!(before.as_deref(), Some("not done: 1 check missing (tests)"));

    world.run_checks(cx).await;
    let after = cx.update(|cx| source.would_hold(TASK, cx)).await;
    assert_eq!(after, None);
    // Nothing was written: no marker, no done record.
    assert_eq!(world.marker(), None);
    assert_eq!(world.done(), None);
}

#[test]
fn every_key_of_a_judgment_rests_on_the_first_key_of_each_tree() {
    let root = TempDir::new().unwrap();
    let make = |name: &str| {
        let path = root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        init_git_repo(&path);
        write_file(&path, "src/lib.rs", "pub fn one() {}\n");
        commit_everything(&path, "start");
        path
    };
    let (app, lib) = (make("app"), make("lib"));
    let indexes = root.path().join("indexes");
    let git = Git::locate().unwrap().isolated();
    let tree = |repo: &str, path: &PathBuf| RepoTree {
        repo: repo.to_string(),
        path: path.clone(),
        env: EnvBase::for_tip(None, TASK, repo),
    };
    let both = || [tree("app", &app), tree("lib", &lib)];
    let key = |keys: &mut TreeKeys, trees: &[RepoTree]| match futures::executor::block_on(
        keys.key(&git, &indexes, trees),
    ) {
        Ok(key) => key,
        Err(_) => panic!("the key is computed"),
    };
    let before = key(&mut TreeKeys::default(), &both());

    // One check's key is taken, then an edit lands in its tree before the done record's key
    // over both repositories is taken.
    let mut keys = TreeKeys::default();
    let app_key = key(&mut keys, &[tree("app", &app)]);
    write_file(&app, "src/lib.rs", "pub fn two() {}\n");
    let union = key(&mut keys, &both());

    assert_eq!(
        union, before,
        "the union rests on the tree the check was keyed on"
    );
    assert_eq!(key(&mut keys, &[tree("app", &app)]), app_key);
    assert_ne!(key(&mut TreeKeys::default(), &both()), before);
}
