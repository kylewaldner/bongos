//! Tests of parameterized runs, the base tree and checks over several repositories: real git in
//! temporary folders with pinned configuration, fake lane trees, and a counter for time.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_checks_model::evidence::{
    CheckState, EvidenceRecord, Freshness, Output, Params, Stale, StartedBy, TreeKind, freshness,
};
use bongos_checks_model::section::Tone;
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_project_repo::Git;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::SettingsStore;

use crate::base_tree::{TempTree, ensure_apart};
use crate::engine::RunChecksRequest;
use crate::engine_tests::World;
use crate::pools_tests::FakeClock;
use crate::test_support::{
    FakeSecond, FakeTrees, commit_everything, git_in, init_git_repo, tree_snapshot, write_file,
};
use crate::tree_key::{TreeKey, combine_trees, tree_key, trees_key};
use crate::trees::TaskTreesRegistry;

const TASK: &str = "l1";

/// `where` and `classify` log where they ran; `both` runs in the repositories `a` and `b`.
const CHECKS: &str = "
[[checks.check]]
name = \"where\"
run = \"checks/where-log\"

[[checks.check]]
name = \"classify\"
run = \"checks/where-log\"

[[checks.check]]
name = \"both\"
run = \"checks/where-log\"
repos = [\"a\", \"b\"]
";

fn git() -> Git {
    Git::locate().expect("git is installed").isolated()
}

/// A world whose task tree is on the branch `task`, one commit past `main`, with an edit and an
/// untracked file not committed. Its scratch folder is inside the world. Returns the world and
/// the merge-base, `main`'s commit.
fn world(cx: &mut TestAppContext, clock: Rc<FakeClock>) -> (World, String) {
    let world = World::with_checks(cx, true, clock, CHECKS);
    let scratch = world.root().join("scratch");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_checks.get_or_insert_default().scratch_dir =
                    Some(scratch.to_string_lossy().into_owned());
            });
        });
    });
    let base = branch_off(&world.tree, "app");
    (world, base)
}

/// Commits `marker.txt` saying `base of <name>` on `main`, then moves to a new branch `task` with
/// a commit saying `tip of <name>`, an uncommitted edit and an untracked file. Returns `main`'s
/// commit.
fn branch_off(tree: &Path, name: &str) -> String {
    write_file(tree, "marker.txt", &format!("base of {name}\n"));
    let ignored = std::fs::read_to_string(tree.join(".gitignore")).unwrap_or_default();
    write_file(tree, ".gitignore", &format!("{ignored}ignored/\n"));
    commit_everything(tree, "base");
    let base = git_in(tree, &["rev-parse", "HEAD"]);
    git_in(tree, &["checkout", "-q", "-b", "task"]);
    write_file(tree, "marker.txt", &format!("tip of {name}\n"));
    write_file(tree, "src/tip.rs", "pub fn tip() {}\n");
    commit_everything(tree, "tip");
    write_file(tree, "src/tip.rs", "pub fn tip() { /* editing */ }\n");
    write_file(tree, "notes.txt", "not committed\n");
    base
}

/// A repository `name` of the task in its own folder, branched off like the task's tree.
fn repository(world: &World, name: &str) -> (PathBuf, String) {
    let tree = world.root().join(format!("repo-{name}"));
    std::fs::create_dir_all(&tree).unwrap();
    init_git_repo(&tree);
    let base = branch_off(&tree, name);
    (tree, base)
}

/// A second tree of the repository at `tree`, on the task's tip, holding an untracked file and
/// an ignored build output.
fn second_tree(world: &World, tree: &Path, name: &str) -> PathBuf {
    let path = world.root().join(format!("second-{name}"));
    git_in(
        tree,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    write_file(&path, "stray.txt", "left by an earlier job\n");
    write_file(&path, "ignored/build.out", "build output\n");
    path
}

fn scratch(world: &World) -> PathBuf {
    world.root().join("scratch")
}

/// The temporary trees and owner files in the scratch folder: every entry but the key indexes.
fn scratch_entries(world: &World) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(scratch(world)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "tree-keys")
        .collect();
    names.sort();
    names
}

fn worktree_count(tree: &Path) -> usize {
    git_in(tree, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

fn params(tree: TreeKind, repeat: Option<u32>, items: &[&str]) -> Params {
    Params {
        items: items.iter().map(|item| item.to_string()).collect(),
        repeat_index: repeat,
        tree: Some(tree),
    }
}

fn request(world: &World, check: &str, params: Option<Params>) -> RunChecksRequest {
    RunChecksRequest {
        params,
        ..world.only(&[check])
    }
}

fn record_folder(record: &EvidenceRecord) -> PathBuf {
    match &record.output {
        Output::Path(path) => PathBuf::from(path),
        other => panic!("no output folder: {other:?}"),
    }
}

/// The lines `where-log` wrote in part `part` of `record`, as `name=value` pairs.
fn logged(record: &EvidenceRecord, part: u32) -> std::collections::BTreeMap<String, String> {
    let text =
        std::fs::read_to_string(record_folder(record).join(format!("part-{part}.log"))).unwrap();
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

/// Resolves once the blocking `where-log` has started in the world's output folder.
fn when_blocked(world: &World) -> futures::channel::oneshot::Receiver<()> {
    let blocked = world.output.join("blocked");
    let (reached, reaching) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !blocked.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the check never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reached.send(()).ok();
    });
    reaching
}

fn block(world: &World) {
    std::fs::create_dir_all(&world.output).unwrap();
    std::fs::write(world.output.join("block"), "").unwrap();
}

#[gpui::test]
async fn base_run_checks_out_the_merge_base_in_a_temporary_tree_removed_afterward(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (world, base) = world(cx, FakeClock::automatic());
    let before = tree_snapshot(&world.tree);

    let report = world
        .run(
            cx,
            request(&world, "where", Some(params(TreeKind::Base, None, &[]))),
        )
        .await;

    assert_eq!(report.results()[0].verdict, Verdict::Pass, "{report:?}");
    let record = &world.records("where")[0];
    assert_eq!(record.tree, TreeKind::Base);
    assert_eq!(record.base.as_deref(), Some(base.as_str()));
    assert!(record.variant.params.is_some());
    let log = logged(record, 1);
    // It ran at the merge-base, in a tree inside the scratch folder, with bongos's own
    // environment.
    assert_eq!(log["marker"], "base of app");
    assert_eq!(log["tree"], "base");
    assert_eq!(log["mark"], "none");
    let ran_in = PathBuf::from(&log["pwd"]);
    assert_eq!(
        ran_in.parent().unwrap(),
        scratch(&world).canonicalize().unwrap()
    );
    // The tree, its owner file and its registration are gone.
    assert!(!ran_in.exists());
    assert!(
        scratch_entries(&world).is_empty(),
        "{:?}",
        scratch_entries(&world)
    );
    assert_eq!(worktree_count(&world.tree), 1);
    // The task's tree, its files and HEAD, is as it was.
    assert_eq!(tree_snapshot(&world.tree), before);
}

#[gpui::test]
async fn base_run_cancelled_removes_its_temporary_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let before = tree_snapshot(&world.tree);
    block(&world);
    let blocked = when_blocked(&world);

    let run = world.run(
        cx,
        request(&world, "where", Some(params(TreeKind::Base, None, &[]))),
    );
    blocked.await.unwrap();
    assert_eq!(
        scratch_entries(&world).len(),
        2,
        "the tree and its owner file"
    );
    assert!(world.engine.update(cx, |engine, _| engine.cancel(TASK)));
    let report = run.await;

    assert!(report.cancelled());
    assert_eq!(
        report.results()[0].verdict,
        Verdict::NoVerdict(NoVerdictReason::Cancelled)
    );
    assert!(
        scratch_entries(&world).is_empty(),
        "{:?}",
        scratch_entries(&world)
    );
    assert_eq!(worktree_count(&world.tree), 1);
    assert_eq!(tree_snapshot(&world.tree), before);
}

#[gpui::test]
async fn a_temporary_tree_dropped_without_its_removal_is_removed(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, base) = world(cx, FakeClock::automatic());
    let protected = [world.tree.as_path()];
    let temp = TempTree::create(
        &git(),
        &world.tree,
        &scratch(&world),
        &base,
        &world.index,
        &protected,
    )
    .await
    .unwrap();
    let path = temp.path().to_path_buf();
    assert_eq!(
        std::fs::read_to_string(path.join("marker.txt")).unwrap(),
        "base of app\n"
    );

    // As when the run's future is dropped.
    drop(temp);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !scratch_entries(&world).is_empty() {
        assert!(std::time::Instant::now() < deadline, "never removed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!path.exists());
    assert_eq!(worktree_count(&world.tree), 1);
}

#[gpui::test]
async fn a_leftover_from_a_crash_is_removed_at_the_next_load(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let scratch = scratch(&world);
    std::fs::create_dir_all(&scratch).unwrap();
    let add = |name: &str| {
        let path = scratch.join(name);
        git_in(
            &world.tree,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    };
    // A crash left a tree whose owner is gone: no owner file, or one naming no live process.
    let orphan = add("01890a5d-ac96-774b-bcce-b302099a8057");
    let dead = add("01890a5d-ac96-774b-bcce-b302099a8058");
    std::fs::write(
        scratch.join("01890a5d-ac96-774b-bcce-b302099a8058.owner"),
        "4294967 1\n",
    )
    .unwrap();
    // A tree this process still owns stays, and so does anything not named like a tree.
    let live = add("01890a5d-ac96-774b-bcce-b302099a8059");
    std::fs::write(
        scratch.join("01890a5d-ac96-774b-bcce-b302099a8059.owner"),
        format!(
            "{} {}\n",
            std::process::id(),
            bongos_workflow::process::start_time(std::process::id()).unwrap_or(0)
        ),
    )
    .unwrap();
    let other = scratch.join("keep-me");
    write_file(&other, "file.txt", "mine\n");
    // A link named like a tree is never followed.
    let outside = world.root().join("outside");
    write_file(&outside, "precious.txt", "keep\n");
    std::os::unix::fs::symlink(
        &outside,
        scratch.join("01890a5d-ac96-774b-bcce-b302099a805a"),
    )
    .unwrap();
    assert_eq!(worktree_count(&world.tree), 4);

    let removed = world
        .engine
        .update(cx, |engine, cx| engine.sweep_scratch(git(), cx))
        .await;

    let canonical_scratch = scratch.canonicalize().unwrap();
    let mut removed: Vec<PathBuf> = removed;
    removed.sort();
    assert_eq!(
        removed,
        [
            canonical_scratch.join("01890a5d-ac96-774b-bcce-b302099a8057"),
            canonical_scratch.join("01890a5d-ac96-774b-bcce-b302099a8058"),
        ]
    );
    assert!(!orphan.exists() && !dead.exists());
    assert!(live.join("marker.txt").exists());
    assert!(other.join("file.txt").exists());
    assert!(outside.join("precious.txt").exists());
    assert_eq!(
        scratch_entries(&world),
        [
            "01890a5d-ac96-774b-bcce-b302099a8059",
            "01890a5d-ac96-774b-bcce-b302099a8059.owner",
            "01890a5d-ac96-774b-bcce-b302099a805a",
            "keep-me",
        ]
    );
    assert_eq!(
        worktree_count(&world.tree),
        2,
        "the task's tree and the live one"
    );
}

#[gpui::test]
async fn a_check_over_two_repositories_runs_in_each_tip_with_its_env(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let (tree_a, _) = repository(&world, "a");
    let (tree_b, _) = repository(&world, "b");
    let trees = FakeTrees::default();
    {
        let mut state = trees.state();
        let path = std::env::var("PATH").unwrap_or_default();
        for (repo, tree) in [("a", &tree_a), ("b", &tree_b)] {
            state.tips.insert(repo.to_string(), tree.clone());
            state.envs.insert(
                repo.to_string(),
                vec![
                    ("PATH".to_string(), path.clone()),
                    ("LANE_MARK".to_string(), format!("tip-{repo}")),
                ],
            );
        }
    }
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    let before = (tree_snapshot(&tree_a), tree_snapshot(&tree_b));

    let report = world.run(cx, request(&world, "both", None)).await;

    assert_eq!(report.results()[0].verdict, Verdict::Pass, "{report:?}");
    let record = world.records("both")[0].clone();
    // One part in each repository, each in its tip with its lane's environment.
    assert_eq!(record.variant.parts, 2);
    let (a, b) = (logged(&record, 1), logged(&record, 2));
    assert_eq!(
        (a["pwd"].as_str(), a["mark"].as_str()),
        (canonical(&tree_a).as_str(), "tip-a")
    );
    assert_eq!(
        (b["pwd"].as_str(), b["mark"].as_str()),
        (canonical(&tree_b).as_str(), "tip-b")
    );
    assert_eq!(
        (a["part"].as_str(), b["part"].as_str()),
        ("1 of 1", "1 of 1")
    );
    // The key is the digest of the sorted (repository, tree) pairs, whatever their order.
    let key = |tree: &Path| {
        let tree = tree.to_path_buf();
        let index = world.index.clone();
        async move { tree_key(&git(), &tree, &index).await.unwrap() }
    };
    let (key_a, key_b) = (key(&tree_a).await, key(&tree_b).await);
    assert_eq!(
        record.tree_key,
        combine_trees(&[("b", key_b.clone()), ("a", key_a.clone())]).0
    );
    assert_ne!(record.tree_key, key_a.0);
    // An edit in either tree makes the pass stale.
    let state = |key: TreeKey| CheckState {
        tree_key: key.0,
        key_inputs: record.key_inputs.clone(),
        digest: record.digest.clone(),
    };
    let pairs = [("a", tree_a.as_path()), ("b", tree_b.as_path())];
    let runner = git();
    let current = || trees_key(&runner, &pairs, &world.index);
    assert_eq!(
        freshness(&record, Some(&state(current().await.unwrap()))),
        Freshness::Fresh
    );
    for tree in [&tree_a, &tree_b] {
        let original = std::fs::read_to_string(tree.join("marker.txt")).unwrap();
        write_file(tree, "marker.txt", "edited\n");
        assert_eq!(
            freshness(&record, Some(&state(current().await.unwrap()))),
            Freshness::Stale(Stale::EarlierTree)
        );
        write_file(tree, "marker.txt", &original);
        assert_eq!(
            freshness(&record, Some(&state(current().await.unwrap()))),
            Freshness::Fresh
        );
    }
    // A tip run asks for no second tree and leaves the trees alone.
    assert!(trees.state().asked.is_empty());
    assert_eq!((tree_snapshot(&tree_a), tree_snapshot(&tree_b)), before);
}

#[gpui::test]
async fn a_base_run_uses_each_repositorys_lease_and_releases_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let (tree_a, base_a) = repository(&world, "a");
    let (tree_b, base_b) = repository(&world, "b");
    let (second_a, second_b) = (
        second_tree(&world, &tree_a, "a"),
        second_tree(&world, &tree_b, "b"),
    );
    let trees = FakeTrees::default();
    {
        let mut state = trees.state();
        let path = std::env::var("PATH").unwrap_or_default();
        for (repo, tree, second) in [("a", &tree_a, &second_a), ("b", &tree_b, &second_b)] {
            state.tips.insert(repo.to_string(), tree.clone());
            state.envs.insert(
                repo.to_string(),
                vec![
                    ("PATH".to_string(), path.clone()),
                    ("LANE_MARK".to_string(), format!("tip-{repo}")),
                ],
            );
            state.second.insert(
                repo.to_string(),
                FakeSecond::Lease {
                    path: second.clone(),
                    env: vec![
                        ("PATH".to_string(), path.clone()),
                        ("LANE_MARK".to_string(), format!("base-{repo}")),
                    ],
                },
            );
        }
    }
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    let before = (
        tree_snapshot(&world.tree),
        tree_snapshot(&tree_a),
        tree_snapshot(&tree_b),
    );

    let report = world
        .run(
            cx,
            request(&world, "both", Some(params(TreeKind::Base, None, &[]))),
        )
        .await;

    assert_eq!(report.results()[0].verdict, Verdict::Pass, "{report:?}");
    let record = world.records("both")[0].clone();
    assert_eq!(
        record.base.as_deref(),
        Some(format!("a={base_a} b={base_b}").as_str())
    );
    let (a, b) = (logged(&record, 1), logged(&record, 2));
    assert_eq!(
        (a["pwd"].as_str(), a["mark"].as_str(), a["marker"].as_str()),
        (canonical(&second_a).as_str(), "base-a", "base of a")
    );
    assert_eq!(
        (b["pwd"].as_str(), b["mark"].as_str(), b["marker"].as_str()),
        (canonical(&second_b).as_str(), "base-b", "base of b")
    );
    // Each lent tree sits detached at its base; untracked files went, build output stayed.
    for (second, base) in [(&second_a, &base_a), (&second_b, &base_b)] {
        assert_eq!(&git_in(second, &["rev-parse", "HEAD"]), base);
        assert!(!second.join("stray.txt").exists());
        assert!(second.join("ignored/build.out").exists());
    }
    // Leased once each, released after the run, and no temporary tree was made.
    let state = trees.state();
    assert_eq!(state.leased, ["a", "b"]);
    assert_eq!(state.released, ["a", "b"]);
    assert!(state.held.is_empty());
    drop(state);
    assert!(scratch_entries(&world).is_empty());
    assert_eq!(
        (
            tree_snapshot(&world.tree),
            tree_snapshot(&tree_a),
            tree_snapshot(&tree_b)
        ),
        before
    );
}

#[gpui::test]
async fn busy_and_unavailable_fall_back_to_a_temporary_tree_at_once(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    // The clock never moves: a run that waited on it would never end.
    let (world, _) = world(cx, FakeClock::manual());
    let trees = FakeTrees::default();
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    let before = tree_snapshot(&world.tree);

    for answer in [FakeSecond::Busy, FakeSecond::Unavailable] {
        trees.state().second.insert("app".to_string(), answer);
        let report = world
            .run(
                cx,
                RunChecksRequest {
                    clean: true,
                    ..request(&world, "where", Some(params(TreeKind::Base, None, &[])))
                },
            )
            .await;

        assert_eq!(report.results()[0].verdict, Verdict::Pass, "{report:?}");
        let record = world.records("where").last().unwrap().clone();
        let log = logged(&record, 1);
        assert_eq!(log["marker"], "base of app");
        assert_eq!(
            PathBuf::from(&log["pwd"]).parent().unwrap(),
            scratch(&world).canonicalize().unwrap()
        );
        assert!(scratch_entries(&world).is_empty());
    }
    let state = trees.state();
    assert_eq!(state.asked, ["app", "app"]);
    assert!(state.leased.is_empty());
    drop(state);
    assert_eq!(tree_snapshot(&world.tree), before);
}

#[gpui::test]
async fn drain_during_a_base_run_releases_the_lease_it_holds(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::manual());
    let second = second_tree(&world, &world.tree, "app");
    let trees = FakeTrees::default();
    trees.state().second.insert(
        "app".to_string(),
        FakeSecond::Lease {
            path: second.clone(),
            env: vec![(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )],
        },
    );
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    block(&world);
    let blocked = when_blocked(&world);

    let run = world.run(
        cx,
        request(&world, "where", Some(params(TreeKind::Base, None, &[]))),
    );
    blocked.await.unwrap();
    assert_eq!(trees.state().leased, ["app"]);
    assert!(
        trees.state().released.is_empty(),
        "held while the check runs"
    );

    let drained = world.engine.update(cx, |engine, cx| engine.drain(cx)).await;

    assert!(drained.is_empty());
    assert_eq!(trees.state().released, ["app"]);
    assert!(trees.state().held.is_empty());
    let report = run.await;
    assert!(report.cancelled());
    assert_eq!(trees.state().released, ["app"], "released once");
}

#[gpui::test]
async fn a_parameterized_pass_satisfies_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let tone = |world: &World, cx: &mut TestAppContext| {
        let status = world
            .engine
            .read_with(cx, |engine, cx| engine.status(TASK, cx))
            .unwrap();
        status
            .section
            .rows
            .iter()
            .find(|row| row.id == "where")
            .map(|row| (row.tone, row.text.clone()))
            .unwrap()
    };

    for params in [
        params(TreeKind::Tip, Some(1), &["one test"]),
        params(TreeKind::Tip, Some(2), &[]),
        params(TreeKind::Base, None, &[]),
    ] {
        let report = world.run(cx, request(&world, "where", Some(params))).await;
        assert_eq!(report.results()[0].verdict, Verdict::Pass);
        let (tone, text) = tone(&world, cx);
        assert_ne!(tone, Tone::Success, "{text}");
    }
    assert_eq!(world.records("where").len(), 3);

    // The same check without parameters satisfies its row.
    world.run(cx, request(&world, "where", None)).await;
    let (tone, text) = tone(&world, cx);
    assert_eq!(tone, Tone::Success, "{text}");
}

#[gpui::test]
async fn the_evidence_file_lists_the_four_reruns(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let node = (
        "run-1".to_string(),
        bongos_workflow_model::NodeKey::new("verify", 1),
    );
    let in_run = |check: &str, params: Option<Params>| RunChecksRequest {
        started_by: StartedBy::Run {
            run_id: node.0.clone(),
            node_key: "verify#1".to_string(),
        },
        node: Some(node.clone()),
        ..request(&world, check, params)
    };

    // Two reruns of one failing test at the tip and two at the base.
    let mut reruns = Vec::new();
    for tree in [TreeKind::Tip, TreeKind::Base] {
        for repeat in [1, 2] {
            let params = params(tree, Some(repeat), &["one test"]);
            let report = world.run(cx, in_run("where", Some(params))).await;
            assert_eq!(report.results()[0].verdict, Verdict::Pass, "{report:?}");
            reruns.push(report.results()[0].record_id.clone().unwrap());
        }
    }
    let report = world.run(cx, in_run("classify", None)).await;
    assert_eq!(report.results()[0].verdict, Verdict::Pass);

    let classify = world.records("classify")[0].clone();
    let copy = record_folder(&classify).join("evidence-copy.json");
    let evidence: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(copy).unwrap()).unwrap();
    let listed: Vec<EvidenceRecord> = serde_json::from_value(evidence["records"].clone()).unwrap();
    let mut parameterized: Vec<String> = listed
        .iter()
        .filter(|record| record.variant.params.is_some())
        .map(|record| record.id.clone())
        .collect();
    parameterized.sort();
    reruns.sort();
    assert_eq!(parameterized, reruns);
    let trees: Vec<(TreeKind, Option<u32>)> = listed
        .iter()
        .filter_map(|record| {
            let params = record.variant.params.as_ref()?;
            Some((record.tree, params.repeat_index))
        })
        .collect();
    assert_eq!(
        trees,
        [
            (TreeKind::Tip, Some(1)),
            (TreeKind::Tip, Some(2)),
            (TreeKind::Base, Some(1)),
            (TreeKind::Base, Some(2)),
        ]
    );
    assert_eq!(evidence["run_id"], "run-1");
    assert_eq!(evidence["tree_key"], classify.tree_key.as_str());
    // The file is gone once the check ended; it was read-only while it lived.
    let leftover: Vec<_> = std::fs::read_dir(&world.output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("evidence-"))
        .collect();
    assert!(leftover.is_empty(), "{leftover:?}");
}

#[test]
fn a_tree_that_overlaps_the_tasks_tree_is_refused() {
    let root = tempfile::TempDir::new().unwrap();
    let task = root.path().join("task");
    let inside = task.join("sub");
    let apart = root.path().join("apart");
    for dir in [&inside, &apart] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let task = task.canonicalize().unwrap();
    let protected = [task.as_path()];

    assert!(ensure_apart(&task, &protected).is_err(), "the same tree");
    assert!(
        ensure_apart(&inside.canonicalize().unwrap(), &protected).is_err(),
        "inside it"
    );
    assert!(
        ensure_apart(&root.path().canonicalize().unwrap(), &protected).is_err(),
        "holding it"
    );
    assert!(ensure_apart(&apart.canonicalize().unwrap(), &protected).is_ok());
}

#[gpui::test]
async fn a_lent_tree_that_is_the_tasks_tree_is_refused_and_left_alone(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (world, _) = world(cx, FakeClock::automatic());
    let trees = FakeTrees::default();
    trees.state().second.insert(
        "app".to_string(),
        FakeSecond::Lease {
            path: world.tree.clone(),
            env: Vec::new(),
        },
    );
    cx.update(|cx| TaskTreesRegistry::set(Rc::new(trees.clone()), cx));
    let before = tree_snapshot(&world.tree);

    let report = world
        .run(
            cx,
            request(&world, "where", Some(params(TreeKind::Base, None, &[]))),
        )
        .await;

    assert!(
        matches!(
            &report.results()[0].verdict,
            Verdict::NoVerdict(NoVerdictReason::StartFailed(why)) if why.contains("never used")
        ),
        "{report:?}"
    );
    assert_eq!(tree_snapshot(&world.tree), before);
    assert_eq!(trees.state().released, ["app"]);
}
