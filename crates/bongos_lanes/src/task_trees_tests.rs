//! A task's trees as the lanes serve them, over real git in a temporary directory: the second
//! tree and its project-listed directories, the tip and its environment per repository, and the
//! exclusive leases of a lane's second tree with the job start that never waits for one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use bongos_checks::{SecondTree, SecondTreeLease, TaskTrees};
use bongos_lanes_model::{
    Author, EnvEdit, JobRecord, LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState,
    RunSubject, TaskId, TokenValues,
};
use bongos_work_model::store::WorkTree;
use gpui::TestAppContext;

use crate::job_trees::{
    CurrentJobTrees, JobRepositories, JobRepository, JobStart, StartedTrees, start_job_trees,
};
use crate::lane_tree::reset_tree;
use crate::lane_tree_tests::{Fixture, RUN_1, RUN_2, RUN_3, git};
use crate::locations::{Place, binding_of};
use crate::locations_tests::table;
use crate::runs::LaneRuns;
use crate::task_trees::{LaneTaskTrees, LaneTreeSetup, ProcessEnv, TopLevelOf};
use crate::test_support::SINCE;

/// `app`'s second tree shares `out` with nothing (it gets `out-base`); `lib`'s lists no
/// directory, so it shares `out` and `cache` with the tip.
const TABLE: &str = r#"schema = 1

[[kind]]
name = "build"
count = 2
[kind.binding]
env = { OUT = "{dir.out}", CACHE = "{dir.cache}", REPO = "{repo}" }
dirs = ["out", "cache"]
trees = [
  { repo = "app", mode = "persistent", base_tree = { dirs = ["out"] } },
  { repo = "lib", mode = "persistent", base_tree = { dirs = [] } },
]
"#;

const PROJECT: &str = "project-1";

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// An injected process environment: the tests never read the real one.
fn process(list: &[(&str, &str)]) -> ProcessEnv {
    let env = pairs(list);
    Arc::new(move || env.clone())
}

/// `leaf` and `leaf-2` sit under `feature`; `loose` and `docs-top` are top-level tasks.
fn top_levels() -> TopLevelOf {
    let tops: BTreeMap<&str, &str> = [
        ("feature", "feature"),
        ("leaf", "feature"),
        ("leaf-2", "feature"),
        ("loose", "loose"),
        ("docs-top", "docs-top"),
    ]
    .into();
    Arc::new(move |task| tops.get(task).map(|top| TaskId::new(*top)))
}

fn owner(lane: &LaneId, state: OwnerState) -> OwnerRecord {
    OwnerRecord {
        lane: Some(lane.clone()),
        state,
        since: SINCE.to_string(),
        by: Author::Lane(lane.clone()),
        reason: None,
        prepared_commit: None,
        refreshed: false,
    }
}

/// `build-1` owns `feature` in `state` and runs a job on `job_task`; `build-2` and `docs-1` are
/// idle and `docs-1` owns `docs-top`.
fn state(state: OwnerState, job_task: &str) -> LanesState {
    let mut running = LaneRecord::new(&build(1), LaneState::Running);
    running.job = Some(JobRecord {
        run_id: RUN_1.to_string(),
        subject: RunSubject::Task(TaskId::new(job_task)),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
    });
    let docs = LaneId::new("docs", 1);
    LanesState {
        owners: [
            (TaskId::new("feature"), owner(&build(1), state)),
            (TaskId::new("docs-top"), owner(&docs, OwnerState::Active)),
        ]
        .into(),
        lanes: [
            (build(1), running),
            (build(2), LaneRecord::new(&build(2), LaneState::Idle)),
            (docs.clone(), LaneRecord::new(&docs, LaneState::Idle)),
        ]
        .into(),
    }
}

fn setup(fixture: &Fixture, profile: Vec<EnvEdit>) -> LaneTreeSetup {
    let table = table(TABLE);
    LaneTreeSetup {
        binding: binding_of(&table, &build(1)).unwrap().clone(),
        locations: fixture.locations.clone(),
        values: TokenValues {
            lane: "build-1".to_string(),
            kind: "build".to_string(),
            lane_index: 1,
            project: "0123456789ab".to_string(),
            repo: String::new(),
            live_lanes: 2,
            cpus: 8,
            dirs: BTreeMap::new(),
        },
        profile,
        forbidden: fixture.forbidden(),
    }
}

/// `build-1`'s setup with its location `name` moved to `path`.
fn moved(fixture: &Fixture, name: &str, path: &Path) -> LaneTreeSetup {
    let mut setup = setup(fixture, Vec::new());
    setup
        .locations
        .items
        .iter_mut()
        .find(|item| item.name == name)
        .unwrap()
        .path = path.to_path_buf();
    setup
}

/// The trees over `runs` and `jobs`, with `build-1` set up from the fixture and `docs-1` bound to
/// no tree at all.
fn trees(
    fixture: &Fixture,
    runs: &LaneRuns,
    jobs: &CurrentJobTrees,
    profile: Vec<EnvEdit>,
    env: ProcessEnv,
) -> LaneTaskTrees {
    let trees = LaneTaskTrees::new(runs.clone(), jobs.clone(), top_levels(), env);
    trees.set_lane(build(1), setup(fixture, profile));
    trees.set_lane(
        LaneId::new("docs", 1),
        LaneTreeSetup {
            binding: Default::default(),
            locations: crate::locations::LaneLocations {
                lane: LaneId::new("docs", 1),
                items: Vec::new(),
            },
            ..setup(fixture, Vec::new())
        },
    );
    trees
}

fn lib_job(fixture: &Fixture, run_id: &str) -> JobRepository {
    JobRepository::for_project("lib", fixture.lib.clone(), None, "main", run_id, 1, None)
}

fn lib_too(_: &JobRecord, _: &WorkTree) -> Vec<String> {
    vec!["lib".to_string()]
}

/// Starts a job of `build-1` in `app` and `lib`, sharing the leases of `trees`.
async fn start_job(fixture: &Fixture, trees: &LaneTaskTrees, run_id: &str) -> StartedTrees {
    let start = JobStart {
        locations: fixture.locations.clone(),
        table_lanes: vec![build(1), build(2)],
        primary: fixture.task_job(run_id, None),
        extra: vec![lib_job(fixture, run_id)],
        save_max_file_bytes: 1000,
        forbidden: fixture.forbidden(),
    };
    let fresh = async |_: &JobRepository| -> Result<PathBuf, String> { Err("unused".to_string()) };
    start_job_trees(&fixture.git, &start, &trees.leases(), &fresh)
        .await
        .unwrap()
}

fn path_of(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.locations.get(name).unwrap().path.clone()
}

fn value(env: &[(String, String)], name: &str) -> Option<String> {
    env.iter()
        .find(|(existing, _)| existing == name)
        .map(|(_, value)| value.clone())
}

fn lease(trees: &LaneTaskTrees, task: &str, repo: &str) -> Box<dyn SecondTreeLease> {
    match trees.second_tree(task, repo) {
        SecondTree::Lease(lease) => lease,
        SecondTree::Busy => panic!("{task} {repo}: busy"),
        SecondTree::Unavailable => panic!("{task} {repo}: unavailable"),
    }
}

fn answer(trees: &LaneTaskTrees, task: &str, repo: &str) -> &'static str {
    match trees.second_tree(task, repo) {
        SecondTree::Lease(_) => "lease",
        SecondTree::Busy => "busy",
        SecondTree::Unavailable => "unavailable",
    }
}

#[gpui::test]
async fn base_tree_dirs_decide_the_second_trees_own_directories(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let base_dirs: Vec<&str> = fixture
        .locations
        .items
        .iter()
        .filter(|item| matches!(item.place, Place::Dir { base: true, .. }))
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(base_dirs, ["out-base"], "dirs = [] gets no directory");
    let out = path_of(&fixture, "out");
    let out_base = path_of(&fixture, "out-base");
    let cache = path_of(&fixture, "cache");
    assert_eq!(
        out_base,
        fixture.base.join("project/lanes/build-1/out-base")
    );

    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf"));
    let jobs = CurrentJobTrees::default();
    let trees = trees(&fixture, &runs, &jobs, Vec::new(), process(&[]));
    let started = start_job(&fixture, &trees, RUN_1).await;
    jobs.insert(build(1), started.trees);
    assert!(started.second_leased.is_empty());

    let app_base = path_of(&fixture, "tree:app:base");
    let lib_base = path_of(&fixture, "tree:lib:base");
    assert_eq!(
        app_base,
        fixture.base.join("data/worktrees/app/lane-build-1-base")
    );
    assert_eq!(git(&app_base, &["status", "--porcelain"]), "");
    assert_eq!(git(&lib_base, &["status", "--porcelain"]), "");

    let app = lease(&trees, "leaf", "app");
    assert_eq!(app.path(), app_base);
    let env = app.env();
    assert_eq!(value(&env, "OUT"), Some(out_base.display().to_string()));
    assert_eq!(value(&env, "CACHE"), Some(cache.display().to_string()));
    assert_eq!(value(&env, "REPO").as_deref(), Some("app"));

    let lib = lease(&trees, "leaf", "lib");
    assert_eq!(lib.path(), lib_base);
    let env = lib.env();
    assert_eq!(value(&env, "OUT"), Some(out.display().to_string()));
    assert_eq!(value(&env, "CACHE"), Some(cache.display().to_string()));
    assert_eq!(value(&env, "REPO").as_deref(), Some("lib"));

    // The tip's own environment has the tip's directories.
    let tip_env = trees.env("leaf", "app");
    assert_eq!(value(&tip_env, "OUT"), Some(out.display().to_string()));
}

#[gpui::test]
async fn the_owning_lane_lends_its_second_tree_to_one_caller_at_a_time(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let runs = LaneRuns::default();
    // The lane's current job is on another leaf of the task it owns.
    runs.set(PROJECT, state(OwnerState::Active, "leaf-2"));
    let jobs = CurrentJobTrees::default();
    let trees = trees(&fixture, &runs, &jobs, Vec::new(), process(&[]));

    // Before the lane's first job start its second tree is not made, and lending makes nothing.
    let app_base = path_of(&fixture, "tree:app:base");
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
    assert!(std::fs::symlink_metadata(&app_base).is_err());
    start_job(&fixture, &trees, RUN_1).await;

    let held = lease(&trees, "leaf", "app");
    assert_eq!(held.path(), app_base);
    assert_eq!(answer(&trees, "leaf", "app"), "busy");
    assert_eq!(answer(&trees, "feature", "app"), "busy");
    // Another repository's second tree is a lease of its own.
    assert_eq!(answer(&trees, "leaf", "lib"), "lease");
    drop(held);
    assert_eq!(answer(&trees, "leaf", "app"), "lease");

    // No lane owns the top-level task, the task is unknown, the lane binds no second tree of
    // the repository, or binds none at all.
    assert_eq!(answer(&trees, "loose", "app"), "unavailable");
    assert_eq!(answer(&trees, "nowhere", "app"), "unavailable");
    assert_eq!(answer(&trees, "leaf", "checkout"), "unavailable");
    assert_eq!(answer(&trees, "docs-top", "app"), "unavailable");

    // Stopped and halted owners lend; a done (for now) or ahead owner does not.
    for (owner_state, expected) in [
        (OwnerState::Stopped, "lease"),
        (OwnerState::Halted, "lease"),
        (OwnerState::Done, "unavailable"),
        (OwnerState::Ahead, "unavailable"),
        (OwnerState::Active, "lease"),
    ] {
        runs.set(PROJECT, state(owner_state, "leaf-2"));
        assert_eq!(answer(&trees, "leaf", "app"), expected, "{owner_state}");
    }

    // An owner naming a lane that no longer exists, or a lane forgotten, lends nothing.
    let mut gone = state(OwnerState::Active, "leaf-2");
    gone.lanes.remove(&build(1));
    runs.set(PROJECT, gone);
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
    runs.set(PROJECT, state(OwnerState::Active, "leaf-2"));
    let kept = lease(&trees, "leaf", "app");
    trees.forget_lane(&build(1));
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
    assert_eq!(kept.path(), app_base, "a lease given stays valid");
    drop(kept);
    assert!(!trees.leases().is_held(&app_base));
}

#[test]
fn racing_callers_get_one_lease_and_a_drop_on_another_thread_frees_it() {
    let fixture = Fixture::build(TABLE, |_| None);
    let app_base = path_of(&fixture, "tree:app:base");
    git(
        &fixture.app,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &app_base.to_string_lossy(),
        ],
    );
    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf-2"));
    let trees = trees(
        &fixture,
        &runs,
        &CurrentJobTrees::default(),
        Vec::new(),
        process(&[]),
    );
    const CALLERS: usize = 8;
    let asked = Arc::new(Barrier::new(CALLERS));
    let answered = Arc::new(Barrier::new(CALLERS));
    let threads: Vec<_> = (0..CALLERS)
        .map(|_| {
            let (trees, asked, answered) = (trees.clone(), asked.clone(), answered.clone());
            std::thread::spawn(move || {
                asked.wait();
                let answer = trees.second_tree("leaf", "app");
                let leased = matches!(answer, SecondTree::Lease(_));
                // Every caller answers while the winner still holds its lease; then each drops
                // its answer on its own thread.
                answered.wait();
                drop(answer);
                leased
            })
        })
        .collect();
    let leased = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|leased| *leased)
        .count();
    assert_eq!(leased, 1);
    assert_eq!(answer(&trees, "leaf", "app"), "lease");
}

#[gpui::test]
async fn a_job_start_skips_a_leased_second_tree_and_the_next_one_resets_it(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let before = fixture.untouched();
    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf"));
    let jobs = CurrentJobTrees::default();
    let trees = trees(&fixture, &runs, &jobs, Vec::new(), process(&[]));
    start_job(&fixture, &trees, RUN_1).await;
    let app_base = path_of(&fixture, "tree:app:base");
    let tip = fixture.tip();

    let held = lease(&trees, "leaf", "app");
    std::fs::write(app_base.join("README.md"), "changed by a check\n").unwrap();
    std::fs::write(app_base.join("left.txt"), "left by a check\n").unwrap();
    std::fs::create_dir_all(app_base.join("target")).unwrap();
    std::fs::write(app_base.join("target/out.bin"), "ignored output\n").unwrap();
    std::fs::write(tip.join("tip.txt"), "left in the tip\n").unwrap();

    // A start while the lease is held does not wait and leaves the leased tree as it is.
    let started = start_job(&fixture, &trees, RUN_2).await;
    assert_eq!(started.second_leased, ["app"]);
    assert_eq!(
        std::fs::read_to_string(app_base.join("README.md")).unwrap(),
        "changed by a check\n"
    );
    assert!(app_base.join("left.txt").exists());
    assert!(!tip.join("tip.txt").exists(), "the tip was reset");
    assert_eq!(answer(&trees, "leaf", "app"), "busy");
    drop(held);

    // The following start resets it: tracked files back, untracked ones removed, ignored kept.
    let started = start_job(&fixture, &trees, RUN_3).await;
    assert!(started.second_leased.is_empty());
    assert_eq!(
        std::fs::read_to_string(app_base.join("README.md")).unwrap(),
        "readme\n"
    );
    assert!(!app_base.join("left.txt").exists());
    assert!(app_base.join("target/out.bin").exists());
    assert_eq!(git(&app_base, &["status", "--porcelain"]), "");
    // The second tree takes no branch; the tip keeps the run branch.
    assert_eq!(git(&app_base, &["branch", "--show-current"]), "");
    assert_eq!(fixture.untouched(), before);
}

#[gpui::test]
async fn tip_and_env_answer_per_repository_with_the_profiles_edits(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    cx.update(|cx| JobRepositories::register(lib_too, cx));
    let job = JobRecord {
        run_id: RUN_1.to_string(),
        subject: RunSubject::Task(TaskId::new("leaf")),
        workflow: "build".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
    };
    let extra = cx.update(|cx| JobRepositories::extra(&job, &WorkTree::default(), "app", cx));
    assert_eq!(extra, ["lib"]);

    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf"));
    let jobs = cx.update(CurrentJobTrees::global);
    let profile = vec![
        EnvEdit::Unset {
            name: "FOO".to_string(),
        },
        EnvEdit::PrependPath {
            dirs: vec![PathBuf::from("/opt/lane/bin")],
        },
    ];
    let env = process(&[("HOME", "/home/u"), ("FOO", "secret"), ("PATH", "/usr/bin")]);
    let trees = trees(&fixture, &runs, &jobs, profile, env);
    assert_eq!(trees.tip("leaf", "app"), None, "no job trees yet");
    assert!(trees.env("leaf", "app").is_empty());

    let started = start_job(&fixture, &trees, RUN_1).await;
    cx.update(|cx| CurrentJobTrees::set(build(1), started.trees.clone(), cx));
    let app_tip = fixture.tip();
    let lib_tip = path_of(&fixture, "tree:lib");
    let shared: &dyn TaskTrees = &trees;
    assert_eq!(shared.tip("leaf", "app"), Some(app_tip));
    assert_eq!(shared.tip("leaf", "lib"), Some(lib_tip));
    assert_eq!(shared.tip("leaf", "checkout"), None);
    // The lane owns `feature`, but its current job is on `leaf` only.
    assert_eq!(shared.tip("feature", "app"), None);
    assert!(shared.env("feature", "app").is_empty());

    let out = path_of(&fixture, "out").display().to_string();
    let out_base = path_of(&fixture, "out-base").display().to_string();
    let cache = path_of(&fixture, "cache").display().to_string();
    let expected = |out: &str, repo: &str| {
        pairs(&[
            ("HOME", "/home/u"),
            ("PATH", "/opt/lane/bin:/usr/bin"),
            ("OUT", out),
            ("CACHE", &cache),
            ("REPO", repo),
        ])
    };
    assert_eq!(shared.env("leaf", "app"), expected(&out, "app"));
    assert_eq!(shared.env("leaf", "lib"), expected(&out, "lib"));
    let leased = lease(&trees, "leaf", "app");
    assert_eq!(leased.env(), expected(&out_base, "app"));

    // The job ends: its trees are forgotten.
    cx.update(|cx| CurrentJobTrees::clear(&build(1), cx));
    assert_eq!(shared.tip("leaf", "app"), None);
    assert!(shared.env("leaf", "app").is_empty());
    assert_eq!(Path::new(leased.path()), path_of(&fixture, "tree:app:base"));
}

/// Adds a linked worktree of `repository` at `path`, detached.
fn add_worktree(repository: &Path, path: &Path) {
    git(
        repository,
        &["worktree", "add", "-q", "--detach", &path.to_string_lossy()],
    );
}

#[gpui::test]
async fn a_second_tree_overlapping_the_tip_is_never_reset_or_lent(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf-2"));
    let jobs = CurrentJobTrees::default();
    let trees = trees(&fixture, &runs, &jobs, Vec::new(), process(&[]));
    start_job(&fixture, &trees, RUN_1).await;
    assert_eq!(answer(&trees, "leaf", "app"), "lease");
    let tip = fixture.tip();
    let app_base = path_of(&fixture, "tree:app:base");
    let job = fixture.task_job(RUN_2, None);

    // The second tree inside the tip, a linked worktree of its own; then the tip inside the
    // second tree.
    let inner = tip.join("base");
    add_worktree(&fixture.app, &inner);
    for (setup, second) in [
        (moved(&fixture, "tree:app:base", &inner), inner.clone()),
        (
            moved(&fixture, "tree:app", &app_base.join("tip")),
            app_base.clone(),
        ),
    ] {
        std::fs::write(second.join("left.txt"), "left\n").unwrap();
        let mut request = fixture.request(&job, true);
        request.locations = setup.locations.clone();
        trees.set_lane(build(1), setup);
        assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
        let error = reset_tree(&fixture.git, &request).await.unwrap_err();
        assert!(error.contains("overlaps"), "{}: {error}", second.display());
        assert!(second.join("left.txt").exists(), "{}", second.display());
    }

    trees.set_lane(build(1), setup(&fixture, Vec::new()));
    assert_eq!(answer(&trees, "leaf", "app"), "lease");
}

#[gpui::test]
async fn a_second_tree_at_a_plain_checkout_or_a_forbidden_place_is_not_lent(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let before = fixture.untouched();
    let runs = LaneRuns::default();
    runs.set(PROJECT, state(OwnerState::Active, "leaf-2"));
    let jobs = CurrentJobTrees::default();
    let trees = trees(&fixture, &runs, &jobs, Vec::new(), process(&[]));
    start_job(&fixture, &trees, RUN_1).await;
    assert_eq!(answer(&trees, "leaf", "app"), "lease");

    // A checkout of its own (a `.git` directory) where the second tree should be: neither lent
    // nor reset.
    let plain = fixture.base.join("data/worktrees/app/plain");
    std::fs::create_dir_all(&plain).unwrap();
    git(&plain, &["init", "-q", "-b", "main"]);
    std::fs::write(plain.join("mine.txt"), "mine\n").unwrap();
    let setup_plain = moved(&fixture, "tree:app:base", &plain);
    let mut request = fixture.request(&fixture.task_job(RUN_2, None), true);
    request.locations = setup_plain.locations.clone();
    trees.set_lane(build(1), setup_plain);
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
    let error = reset_tree(&fixture.git, &request).await.unwrap_err();
    assert!(error.contains("not a worktree of"), "{error}");
    assert!(plain.join("mine.txt").exists());

    // A primary checkout, a task worktree or another lane's tree (both linked worktrees, which
    // only the forbidden places refuse) is never lent.
    let other_lane = fixture.all[1].get("tree:app").unwrap().path.clone();
    add_worktree(&fixture.app, &other_lane);
    for path in [
        fixture.app.clone(),
        fixture.checkout.clone(),
        fixture.task_worktree.clone(),
        other_lane,
    ] {
        trees.set_lane(build(1), moved(&fixture, "tree:app:base", &path));
        assert_eq!(
            answer(&trees, "leaf", "app"),
            "unavailable",
            "{}",
            path.display()
        );
    }
    assert_eq!(fixture.untouched(), before);

    trees.set_lane(build(1), setup(&fixture, Vec::new()));
    assert_eq!(answer(&trees, "leaf", "app"), "lease");
}
