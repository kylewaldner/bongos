//! Tests of the done decider for groups: a group is Done when its closing checks pass on its
//! last child's tree, the decider runs the missing ones itself on that recorded tree (written out
//! under the scratch folder and removed after), once per key, and keeps the trees with refs.
//! Checks run through the engine on real git trees; time is a counter, never the wall clock.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bongos_checks_model::evidence::StartedBy;
use bongos_checks_model::verdict::Verdict;
use bongos_checks_model::{EvidenceRecord, effective_set};
use bongos_project_repo::{LandingResult, ProjectRepo, StateApplyHooks, StateWriter};
use bongos_project_repo_model::{DefinitionTree, TaskStateRecord};
use bongos_work_model::Status;
use gpui::{BorrowAppContext as _, Entity, TestAppContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::definitions::named_sets;
use crate::engine::{ChecksEngine, RunChecksRequest};
use crate::evidence_store::{DoneRecord, done_record, put_done, scan_records, task_prefix};
use crate::group_decider::{CHILDREN_HELD, as_found};
use crate::live::HoldRow;
use crate::owed::{ChecksOwedHook, FolderSets, LastRow, OwedMarker, put_marker};
use crate::pin::master_definitions;
use crate::pools_tests::FakeClock;
use crate::settings::TaskChecksSettings;
use crate::task_source::{TaskChecks, TaskSource};
use crate::task_tree::RunWorktrees;
use crate::test_support::{
    commit_everything, definition_toml, fixture_project_with_hooks, git_in, init_git_repo, land,
    main_commit, new_copy, person, write_definition, write_file,
};
use crate::{LAST_NAMESPACE, OWED_NAMESPACE, read_hold_rows};

const PROJECT: &str = "project-1";
const TESTS: &str = "\n[[checks.check]]\nname = \"tests\"\ncommand = [\"true\"]\n";
/// A closing check that passes only where the files of the tree were written out.
const CLOSING: &str =
    "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"test\", \"-f\", \"src/lib.rs\"]\n";
const FAILING_CLOSING: &str = "\n[[checks.closing]]\nname = \"full\"\ncommand = [\"false\"]\n";

#[derive(Default)]
struct Source {
    tasks: RefCell<BTreeMap<String, TaskChecks>>,
    repositories: RefCell<BTreeMap<String, PathBuf>>,
}

impl TaskSource for Source {
    fn task(&self, task_id: &str, _: &gpui::App) -> Option<TaskChecks> {
        self.tasks.borrow().get(task_id).cloned()
    }

    fn done_tasks(&self, _: &gpui::App) -> HashSet<String> {
        HashSet::new()
    }

    fn code_repository(&self, name: &str, _: &gpui::App) -> Option<PathBuf> {
        self.repositories.borrow().get(name).cloned()
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

/// Fields drop in order: the engine and the project go before the folder holding them.
struct World {
    engine: Entity<ChecksEngine>,
    source: Rc<Source>,
    repo: ProjectRepo,
    /// The primary checkout of the code repository `app`, where every task's tree lives.
    app: PathBuf,
    scratch: PathBuf,
    root: TempDir,
}

impl World {
    /// The project with `tasks` (folder under `definitions/`, id, checks table) under its top
    /// group, the code repository `app`, the verified commit recorded, the owed hook on its
    /// writer, and the engine with the setting on. No task has a tree yet.
    fn new(cx: &mut TestAppContext, tasks: &[(&str, &str, &str)]) -> Self {
        let root = TempDir::new().unwrap();
        let repository = root.path().join("project/repository");
        let hooks = StateApplyHooks::new();
        let hook = Arc::new(ChecksOwedHook::new());
        hook.set_enabled(true);
        hook.set_named_sets(Arc::new(FolderSets(repository.join("definitions"))));
        hooks.register(hook);
        let repo = fixture_project_with_hooks(&root, &hooks);
        for (folder, id, checks) in tasks {
            write_file(
                &repository.join("definitions"),
                &format!("{folder}/task.toml"),
                &definition_toml(id, id, checks),
            );
        }
        commit_everything(&repository, "add the group");
        let app = root.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        init_git_repo(&app);
        write_file(&app, "src/lib.rs", "pub fn one() {}\n");
        commit_everything(&app, "start");
        let scratch = root.path().join("scratch");
        let source = Rc::new(Source::default());
        source
            .repositories
            .borrow_mut()
            .insert("app".to_string(), app.clone());
        let counter = Arc::new(AtomicU64::new(0));
        let scratch_setting = scratch.to_string_lossy().to_string();
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
                    checks.scratch_dir = Some(scratch_setting);
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
            app,
            scratch,
            root,
        };
        world.record_verified();
        for (_, id, _) in tasks {
            world.know(id, &|_| false);
        }
        world
    }

    /// Tells the task source about `id`, its set judged with `done` saying which tasks are Done.
    fn know(&self, id: &str, done: &dyn Fn(&str) -> bool) {
        let definitions = master_definitions(&self.repo);
        let tree = DefinitionTree::parse_dir(&definitions).expect("the definitions parse");
        let worktree = self
            .source
            .tasks
            .borrow()
            .get(id)
            .and_then(|task| task.worktree.clone());
        let task = TaskChecks {
            task_id: id.to_string(),
            name: id.to_string(),
            repo: Some("app".to_string()),
            worktree,
            set: effective_set(id, &tree, &named_sets(&definitions), done),
            project: self.repo.clone(),
            base_branch: None,
            output_dir: self.root.path().join("output").join(id),
            index_dir: self.root.path().join("index"),
        };
        self.source.tasks.borrow_mut().insert(id.to_string(), task);
    }

    /// Gives `id` a worktree of `app` holding `contents` in `src/lib.rs`, not committed.
    fn worktree(&self, id: &str, contents: &str) -> PathBuf {
        let path = self.root.path().join("trees").join(id);
        git_in(
            &self.app,
            &["worktree", "add", "-q", "-b", id, path.to_str().unwrap()],
        );
        write_file(&path, "src/lib.rs", contents);
        self.source
            .tasks
            .borrow_mut()
            .get_mut(id)
            .expect("a known task")
            .worktree = Some(path.clone());
        path
    }

    fn record_verified(&self) {
        let commit = main_commit(&self.repo);
        let definitions = Arc::new(
            DefinitionTree::parse_dir(&master_definitions(&self.repo)).expect("they parse"),
        );
        self.writer()
            .transact(|tx| tx.set_verified_commit(&commit, definitions.clone()))
            .unwrap();
    }

    /// Lands a change that touches no check, moving the verified commit.
    fn land_unrelated(&self, name: &str, text: &str) {
        let author = person();
        let copy = new_copy(&self.repo, name, &author);
        write_definition(&copy, "plan.md", text);
        let landed = land(&self.repo, &copy, &author);
        assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
    }

    fn writer(&self) -> Arc<StateWriter> {
        self.repo.state().unwrap().writer().unwrap().clone()
    }

    fn set(&self, ids: &[&str], status: Status) {
        self.writer()
            .transact(|tx| {
                for id in ids {
                    tx.set_task(
                        id,
                        TaskStateRecord {
                            status,
                            ..TaskStateRecord::default()
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn finish(&self, ids: &[&str]) {
        self.set(ids, Status::Done);
    }

    async fn run_checks(&self, id: &str, cx: &mut TestAppContext) {
        let report = self
            .engine
            .update(cx, |engine, cx| {
                engine.run(RunChecksRequest::person(id), cx)
            })
            .await;
        assert!(
            !report.results().is_empty()
                && report
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

    fn group_runs(&self, cx: &mut TestAppContext) -> usize {
        self.engine
            .read_with(cx, |engine, _| engine.decider.group_runs.len())
    }

    /// Decides, lets every group run that started end, and decides again, until a decision
    /// starts nothing.
    async fn settle(&self, cx: &mut TestAppContext) {
        for _ in 0..8 {
            self.decide(cx).await;
            if self.group_runs(cx) == 0 {
                return;
            }
            let ended = self
                .engine
                .read_with(cx, |engine, _| engine.group_runs_ended());
            ended.await;
        }
        panic!("the decisions do not settle");
    }

    fn done(&self, id: &str) -> Option<DoneRecord> {
        self.writer().read(|read| done_record(read, id)).unwrap()
    }

    fn hold(&self, id: &str) -> Option<HoldRow> {
        self.writer().read(read_hold_rows).unwrap().remove(id)
    }

    fn marker(&self, id: &str) -> Option<OwedMarker> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(OWED_NAMESPACE)
                    .get(id)?
                    .map(|value| value.try_into().unwrap()))
            })
            .unwrap()
    }

    fn last(&self, group: &str) -> Option<LastRow> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(LAST_NAMESPACE)
                    .get(group)?
                    .map(|value| value.try_into().unwrap()))
            })
            .unwrap()
    }

    fn records(&self, id: &str) -> Vec<EvidenceRecord> {
        self.writer()
            .read(|read| scan_records(read, &task_prefix(id)))
            .unwrap()
    }

    /// The tree the ref `name` of `app` points at, if it exists.
    fn kept(&self, name: &str) -> Option<String> {
        let refs = git_in(
            &self.app,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        refs.lines().find_map(|line| {
            let (refname, object) = line.split_once(' ')?;
            (refname == name).then(|| object.to_string())
        })
    }

    /// What is left in the scratch folder but the folder of tree key indexes.
    fn scratch_left(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.scratch) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "tree-keys")
            .collect()
    }
}

fn missing(count: u32) -> HoldRow {
    HoldRow {
        reason: crate::decider::missing_reason(count as usize),
        unsatisfied: Some(count),
    }
}

/// `grp` with the closing check `closing` over the leaves `c1` and `c2`, each with `checks`.
fn group(
    closing: &'static str,
    checks: &'static str,
) -> [(&'static str, &'static str, &'static str); 3] {
    [
        ("tasks/grp", "grp", closing),
        ("tasks/grp/tasks/c1", "c1", checks),
        ("tasks/grp/tasks/c2", "c2", checks),
    ]
}

/// The tree key of `path` as the decider computes it.
fn key_of(world: &World, path: &Path) -> String {
    futures::executor::block_on(crate::tree_key::tree_key(
        world.repo.git(),
        path,
        &world.root.path().join("index"),
    ))
    .unwrap()
    .as_str()
    .to_string()
}

fn assert_group_run(world: &World, group: &str, verdict: Verdict) {
    let records = world.records(group);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].started_by, StartedBy::GroupDecision);
    assert_eq!(records[0].verdict, verdict);
}

#[gpui::test]
async fn a_group_whose_last_open_child_was_dropped_shows_the_missing_closing_check(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    world.decide(cx).await;
    world.finish(&["c1"]);
    world.decide(cx).await;
    assert_eq!(
        world.marker("c1"),
        None,
        "c2 is still open: c1 carries nothing"
    );
    let keyed = world.last("grp").expect("c1 is recorded");
    assert_eq!(keyed.id, "c1");

    // Dropping the last open child finishes the group, which nobody carried.
    world.set(&["c2"], Status::Cancelled);
    let marker = world.marker("grp").expect("the group is marked");
    assert_eq!(marker.last_child.map(|last| last.id).as_deref(), Some("c1"));
    world.decide(cx).await;
    assert_eq!(world.done("grp"), None);
    assert_eq!(world.hold("grp"), Some(missing(1)));

    world.settle(cx).await;
    let done = world
        .done("grp")
        .expect("Done after the group's run passed");
    assert_eq!(Some(done.tree_key.clone()), keyed.tree_key);
    assert_eq!(
        done.tree_key,
        key_of(&world, &world.root.path().join("trees/c1"))
    );
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(world.hold("grp"), None);
}

#[gpui::test]
async fn a_group_whose_last_child_carried_its_closing_check_is_done_after_the_pass(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c2", "pub fn c2() {}\n");
    world.finish(&["c1"]);
    // c2 finishes last while c1 is Done: it carries the closing check and passes it.
    world.know("c2", &|id| id == "c1");
    world.run_checks("c2", cx).await;
    world.finish(&["c2"]);
    assert_eq!(
        world.marker("c2").map(|marker| marker.closing_from),
        Some(vec!["grp".to_string()])
    );

    world.settle(cx).await;

    let child = world.done("c2").expect("c2 is Done");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(done.tree_key, child.tree_key);
    assert_eq!(done.evidence, child.evidence);
    assert!(world.records("grp").is_empty(), "the group ran nothing");
}

#[gpui::test]
async fn siblings_closing_together_leave_the_closing_check_to_a_group_run_with_no_person(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, TESTS));
    world.worktree("c1", "pub fn c1() {}\n");
    world.worktree("c2", "pub fn c2() {}\n");
    world.run_checks("c1", cx).await;
    world.run_checks("c2", cx).await;
    world.finish(&["c1", "c2"]);
    for id in ["c1", "c2"] {
        assert_eq!(
            world.marker(id).map(|marker| marker.closing_from),
            Some(Vec::new()),
            "{id} carries nothing"
        );
    }

    world.settle(cx).await;

    let last = world.done("c2").expect("c2 is Done");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(done.tree_key, last.tree_key, "run on the last child's tree");
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(done.evidence, vec![world.records("grp")[0].id.clone()]);
    assert_eq!(world.scratch_left(), Vec::<String>::new());
}

#[gpui::test]
async fn a_group_whose_children_owe_nothing_records_its_last_child_and_runs_on_its_tree(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    assert_eq!(world.marker("c1"), None);
    assert_eq!(world.marker("c2"), None);
    let marker = world.marker("grp").expect("the group is marked");
    assert_eq!(marker.last_child.map(|last| last.id).as_deref(), Some("c2"));

    world.settle(cx).await;

    let last = world.last("grp").expect("the last child row");
    assert_eq!(last.id, "c2");
    assert_eq!(
        last.tree_key.as_deref(),
        Some(key_of(&world, &tree).as_str())
    );
    assert_eq!(last.found_at, None, "computed right when c2 was recorded");
    let done = world.done("grp").expect("the group is Done");
    assert_eq!(Some(done.tree_key), last.tree_key);
    assert_group_run(&world, "grp", Verdict::Pass);
    assert_eq!(
        world.scratch_left(),
        Vec::<String>::new(),
        "the scratch folder is removed"
    );
    // The tree is kept for the group by a ref on c2.
    assert_eq!(world.kept("refs/bongos/checks/c2"), last.tree_key);
}

#[gpui::test]
async fn a_failing_group_run_holds_the_group_and_later_verified_commit_changes_start_nothing(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(FAILING_CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);

    world.settle(cx).await;

    assert_eq!(world.done("grp"), None);
    assert_eq!(world.hold("grp"), Some(missing(1)));
    assert_eq!(world.scratch_left(), Vec::<String>::new());
    let key = key_of(&world, &tree);
    assert_eq!(world.kept("refs/bongos/checks/c2"), Some(key.clone()));
    // A ref no record backs, left behind.
    git_in(
        &world.app,
        &["update-ref", "refs/bongos/checks/ghost", &key],
    );

    for (name, text) in [("amber-otter-0001", "one\n"), ("bold-crane-0002", "two\n")] {
        let before = main_commit(&world.repo);
        world.land_unrelated(name, text);
        assert_ne!(main_commit(&world.repo), before);
        world.decide(cx).await;
        assert_eq!(world.group_runs(cx), 0, "nothing starts on the same key");
    }

    assert_group_run(&world, "grp", Verdict::Fail);
    assert_eq!(world.hold("grp"), Some(missing(1)));
    assert_eq!(
        world.kept("refs/bongos/checks/ghost"),
        None,
        "the orphan ref is deleted"
    );
    assert_eq!(
        world.kept("refs/bongos/checks/c2"),
        Some(key),
        "a backed ref stays"
    );
}

#[gpui::test]
async fn with_the_last_childs_worktree_removed_the_group_still_becomes_done_unattended(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(
        cx,
        &[
            ("tasks/grp", "grp", CLOSING),
            ("tasks/grp/tasks/c1", "c1", TESTS),
            ("tasks/grp/tasks/c2", "c2", ""),
        ],
    );
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    let key = key_of(&world, &tree);
    world.decide(cx).await;
    world.finish(&["c1", "c2"]);
    world.decide(cx).await;
    // c1 still owes its checks; c2's tree was keyed as it finished.
    assert_eq!(world.hold("grp"), Some(HoldRow::new(CHILDREN_HELD)));
    assert_eq!(
        world.last("grp").and_then(|last| last.tree_key),
        Some(key.clone())
    );

    // c2's branch merges and its worktree goes, before the group's checks run.
    git_in(
        &world.app,
        &["worktree", "remove", "--force", tree.to_str().unwrap()],
    );
    assert!(!tree.exists());
    world.run_checks("c1", cx).await;

    world.settle(cx).await;

    let done = world.done("grp").expect("Done on the kept tree");
    assert_eq!(done.tree_key, key);
    assert_group_run(&world, "grp", Verdict::Pass);
}

#[gpui::test]
async fn an_empty_done_key_is_no_key_and_the_tree_is_keyed_as_found(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, &group(CLOSING, ""));
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.finish(&["c1", "c2"]);
    // c2 owed checks once, and its done record says no checks were left: no key.
    world
        .writer()
        .transact(|tx| {
            put_marker(tx, "c2", &OwedMarker::default())?;
            put_done(
                tx,
                "c2",
                &DoneRecord {
                    tree_key: String::new(),
                    generation: 9,
                    at: "2026-03-01T09:00:00Z".to_string(),
                    evidence: Vec::new(),
                },
            )
        })
        .unwrap();

    world.decide(cx).await;

    let last = world.last("grp").expect("the last child row");
    let key = key_of(&world, &tree);
    assert_eq!(last.tree_key.as_deref(), Some(key.as_str()));
    let at = last.found_at.clone().expect("keyed as found");
    let hold = world.hold("grp").expect("held while its check runs");
    assert!(hold.reason.ends_with(&as_found("c2", &at)), "{hold:?}");

    world.settle(cx).await;
    assert_eq!(world.done("grp").map(|done| done.tree_key), Some(key));
}

#[gpui::test]
async fn a_nested_group_uses_its_own_last_childs_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let inner_closing =
        "\n[[checks.closing]]\nname = \"inner\"\ncommand = [\"test\", \"-f\", \"src/lib.rs\"]\n";
    let world = World::new(
        cx,
        &[
            ("tasks/outer", "outer", CLOSING),
            ("tasks/outer/tasks/a1", "a1", ""),
            ("tasks/outer/tasks/b-in", "b-in", inner_closing),
            ("tasks/outer/tasks/b-in/tasks/c1", "c1", ""),
            ("tasks/outer/tasks/b-in/tasks/c2", "c2", ""),
        ],
    );
    world.worktree("a1", "pub fn a1() {}\n");
    world.worktree("c1", "pub fn c1() {}\n");
    let tree = world.worktree("c2", "pub fn c2() {}\n");
    world.decide(cx).await;
    world.finish(&["a1", "c1", "c2"]);
    assert_eq!(
        world.last("b-in").map(|last| last.id).as_deref(),
        Some("c2")
    );
    assert_eq!(
        world.last("outer").map(|last| last.id).as_deref(),
        Some("b-in")
    );

    world.settle(cx).await;

    let key = key_of(&world, &tree);
    let inner = world.done("b-in").expect("the inner group is Done");
    let outer = world.done("outer").expect("the outer group is Done");
    assert_eq!(inner.tree_key, key);
    assert_eq!(outer.tree_key, key, "the outer group ran on c2's tree");
    assert_group_run(&world, "b-in", Verdict::Pass);
    assert_group_run(&world, "outer", Verdict::Pass);
}
