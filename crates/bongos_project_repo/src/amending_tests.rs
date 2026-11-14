use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bongos_project_repo_model::Author;
use bongos_work_model::TodoState;
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::oneshot;
use futures::executor::block_on;
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::check_fakes::{FakeAmender, FakeCheck, replace_in, write_in};
use crate::landing::ProbePoint;
use crate::test_support::{
    add_task, fixture_project, land, landed, main_commit, new_copy, request, run_git, start,
    task_with_todos, write_definition,
};
use crate::{CheckPhase, LandMode, LandRequest, LandingEvent, LandingResult, UNSAVED_EDITS};

fn head(copy: &crate::WorkingCopy, repo: &crate::ProjectRepo) -> String {
    block_on(repo.git().rev_parse(&copy.path, "HEAD")).unwrap()
}

#[test]
fn an_amending_check_adding_an_edge_is_committed_and_the_other_checks_run_on_the_amended_tree() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_amending_check(Arc::new(FakeAmender::new(
        "edges",
        1,
        replace_in(
            "tasks/alpha/task.toml",
            "kind = \"other\"\n",
            "kind = \"other\"\nafter = [{ task = \"backfill\", unblock = \"merged\" }]\n",
        ),
    )));
    let recorder = Arc::new(FakeCheck::new("recorder", CheckPhase::BeforeQueue));
    repo.add_landing_check(recorder.clone());
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");

    let commit = landed(land(&repo, &copy, &author));
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_eq!(row.amend_commits[0].check, "edges");
    // The amend commit is on the change's branch with its label as the subject.
    assert_eq!(
        run_git(
            &repo,
            &["log", "-1", "--format=%s", &row.amend_commits[0].commit]
        ),
        "amend: edges edit"
    );
    let landed_tree = run_git(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    assert_eq!(
        run_git(
            &repo,
            &[
                "rev-parse",
                &format!("{}^{{tree}}", row.amend_commits[0].commit)
            ]
        ),
        landed_tree
    );
    // The before-queue check ran once, on the amended tree, which is the tree that landed.
    let seen = recorder.seen.lock().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].tree, landed_tree);
    assert!(
        seen[0]
            .alpha
            .as_deref()
            .unwrap()
            .contains("task = \"backfill\"")
    );
    // Its materialized tree is gone once the checks completed.
    assert!(!seen[0].root.exists());
    assert!(
        run_git(&repo, &["show", "main:definitions/tasks/alpha/task.toml"])
            .contains("task = \"backfill\"")
    );
}

#[test]
fn an_amending_check_that_amends_twice_in_a_row_gives_no_verdict() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let count = Arc::new(AtomicUsize::new(0));
    let mut check = FakeAmender::new(
        "restless",
        5,
        Arc::new(move |writable: &std::path::Path| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            std::fs::write(writable.join("definitions/stamp.txt"), format!("{n}\n")).unwrap();
            true
        }),
    );
    check.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(check));
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);

    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::NoVerdict {
            reason: format!("restless: {}", crate::KEEPS_AMENDING)
        }
    );
    assert_eq!(main_commit(&repo), old);
    // Only the first amendment was committed.
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_eq!(head(&copy, &repo), row.amend_commits[0].commit);
}

#[test]
fn amendments_outside_their_bounds_give_no_verdict_and_commit_nothing() {
    let started_tags = || {
        replace_in(
            "tasks/backfill/task.toml",
            "kind = \"other\"\n",
            "kind = \"other\"\nareas = [\"billing\"]\n",
        )
    };
    let cases = [
        (
            "goal",
            replace_in(
                "tasks/alpha/task.toml",
                "goal = \"alpha\"",
                "goal = \"other\"",
            ),
            "goal",
        ),
        (
            "project",
            write_in(
                "project.toml",
                "schema = 1\nid = \"project-1\"\ntitle = \"Renamed\"\n",
            ),
            "project.toml",
        ),
        (
            "workflows",
            write_in("workflows/review/workflow.toml", "name = \"review\"\n"),
            "workflows/review/workflow.toml",
        ),
        ("started", started_tags(), "backfill"),
    ];
    for (case, edit, named) in cases {
        let root = TempDir::new().unwrap();
        let repo = fixture_project(&root);
        repo.add_amending_check(Arc::new(FakeAmender::new("rebalance", 1, edit)));
        let author = Author::agent("chat 1");
        let copy = new_copy(&repo, "amber-otter-0001", &author);
        add_task(&copy, "alpha", "a1");
        // The author's part redefines `backfill`, which has work.
        write_definition(
            &copy,
            "tasks/backfill/task.toml",
            &crate::test_support::definition_toml(
                "l1",
                "Backfill all",
                "other",
                &[("t1", "implement"), ("t2", "review"), ("t3", "check")],
            ),
        );
        repo.writer_or_error()
            .unwrap()
            .transact(|tx| tx.set_task("l1", task_with_todos(&[("t1", TodoState::Done)])))
            .unwrap();
        let old = main_commit(&repo);

        let result = land(&repo, &copy, &author);
        let LandingResult::NoVerdict { reason } = &result else {
            panic!("{case}: expected no verdict, got {result:?}");
        };
        assert!(
            reason.starts_with("rebalance: amendment outside its bounds"),
            "{case}: {reason}"
        );
        assert!(reason.contains(named), "{case}: {reason}");
        assert_eq!(main_commit(&repo), old, "{case}");
        let row = repo.change(&copy.change_id).unwrap().unwrap();
        assert!(row.amend_commits.is_empty(), "{case}");
        assert_eq!(
            run_git(&repo, &["log", "-1", "--format=%s", &row.branch]),
            "land it",
            "{case}"
        );
        assert!(block_on(repo.git().is_clean(&copy.path)).unwrap(), "{case}");
    }
}

#[test]
fn dropping_amendments_while_the_working_copy_has_unsaved_edits_waits_and_loses_nothing() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let mut stamp = FakeAmender::new("stamp", 10, write_in("stamp.txt", "stamped\n"));
    stamp.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(stamp));
    let author = Author::agent("chat 1");
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");
    let typed = second.path.join("definitions/documents/mine.md");

    // The first change holds the queue until the second, amended, waits behind it; then the
    // person types into the second's working copy and the first lands, moving master.
    let (paused_tx, paused_rx) = oneshot::channel::<()>();
    let (open_tx, open_rx) = oneshot::channel::<()>();
    let paused_tx = Mutex::new(Some(paused_tx));
    let open_rx = Mutex::new(Some(open_rx));
    let first_id = first.change_id.clone();
    repo.set_probe(Arc::new(move |point, change| {
        if point == ProbePoint::InQueue && change == first_id {
            let paused = paused_tx.lock().take();
            let open = open_rx.lock().take();
            return async move {
                if let Some(paused) = paused {
                    paused.send(()).ok();
                }
                if let Some(open) = open {
                    open.await.ok();
                }
                false
            }
            .boxed();
        }
        async { false }.boxed()
    }));
    let mut events = repo.landing_events();
    let second_id = second.change_id.clone();
    let land_first = repo.land_now(&first.change_id, request("first", &author), start());
    let land_second = async {
        paused_rx.await.unwrap();
        repo.land_now(&second.change_id, request("second", &author), start())
            .await
    };
    let typed_in = typed.clone();
    let watch = async {
        while let Some((change, event)) = events.next().await {
            if change == second_id && event == (LandingEvent::Queued { position: 1 }) {
                std::fs::create_dir_all(typed_in.parent().unwrap()).unwrap();
                std::fs::write(&typed_in, "my notes\n").unwrap();
                open_tx.send(()).unwrap();
                break;
            }
        }
    };
    let (first_result, second_result, ()) =
        block_on(async { futures::join!(land_first, land_second, watch) });
    landed(first_result.unwrap());
    assert_eq!(
        second_result.unwrap(),
        LandingResult::NoVerdict {
            reason: UNSAVED_EDITS.to_string()
        }
    );
    // Nothing is lost: the typed file, and the amend commit still on the branch.
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), "my notes\n");
    let row = repo.change(&second.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_eq!(head(&second, &repo), row.amend_commits[0].commit);
}

#[test]
fn a_branch_without_a_worktree_drops_its_amendments_by_compare_and_swap() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let mut stamp = FakeAmender::new("stamp", 1, write_in("stamp.txt", "stamped\n"));
    stamp.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(stamp));
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let checked = block_on(repo.land_now(
        &copy.change_id,
        LandRequest {
            meta: bongos_project_repo_model::ChangeMeta::new("check", author),
            mode: LandMode::CheckOnly,
        },
        start(),
    ))
    .unwrap();
    assert!(
        matches!(checked, LandingResult::Checked { .. }),
        "{checked:?}"
    );
    let mut row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    let author_commit = run_git(&repo, &["rev-parse", &format!("{}^", row.branch)]);
    // As a quick edit: the branch has no worktree.
    run_git(
        &repo,
        &["worktree", "remove", "--force", copy.path.to_str().unwrap()],
    );

    let writer = repo.writer_or_error().unwrap();
    let ended =
        block_on(repo.drop_amendments(&writer, &mut row, &copy.path, &crate::Identity::default()))
            .unwrap();
    assert_eq!(ended, None);
    assert_eq!(run_git(&repo, &["rev-parse", &row.branch]), author_commit);
    assert!(
        repo.change(&copy.change_id)
            .unwrap()
            .unwrap()
            .amend_commits
            .is_empty()
    );
}

#[test]
fn scratch_copies_left_by_a_former_process_are_removed_at_open() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let amending = repo.location().state.join("amending");
    let registered = amending.join("amber-otter-0001");
    let plain = amending.join("calm-heron-0002");
    std::fs::create_dir_all(&amending).unwrap();
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            registered.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("left.txt"), "left\n").unwrap();
    let pins = repo.location().state.join("pins/abc");
    std::fs::create_dir_all(&pins).unwrap();

    block_on(repo.recover_landing()).unwrap();
    assert!(!registered.exists());
    assert!(!plain.exists());
    assert!(!pins.exists());
    assert!(
        !run_git(&repo, &["worktree", "list"]).contains("amber-otter-0001"),
        "the scratch worktree is still registered"
    );
}

#[test]
fn an_amend_commit_the_working_copy_cannot_fast_forward_to_is_not_listed() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    // While the check writes its stamp, the person creates the same file in the working copy,
    // which is in the way of the fast-forward.
    let in_copy = copy.path.join("definitions/stamp.txt");
    let mut stamp = FakeAmender::new(
        "stamp",
        1,
        Arc::new(move |writable: &std::path::Path| {
            std::fs::write(writable.join("definitions/stamp.txt"), "stamped\n").unwrap();
            std::fs::write(&in_copy, "mine\n").unwrap();
            true
        }),
    );
    stamp.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(stamp));
    let old = main_commit(&repo);

    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::NoVerdict {
            reason: UNSAVED_EDITS.to_string()
        }
    );
    assert_eq!(main_commit(&repo), old);
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert!(row.amend_commits.is_empty(), "{:?}", row.amend_commits);
    assert_eq!(
        run_git(&repo, &["log", "-1", "--format=%s", &row.branch]),
        "land it"
    );
    assert_eq!(
        std::fs::read_to_string(copy.path.join("definitions/stamp.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_listed_amend_commit_the_branch_never_got_is_skipped_when_the_amendments_are_dropped() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let mut stamp = FakeAmender::new("stamp", 1, write_in("stamp.txt", "stamped\n"));
    stamp.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(stamp));
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let checked = block_on(repo.land_now(
        &copy.change_id,
        LandRequest {
            meta: bongos_project_repo_model::ChangeMeta::new("check", author),
            mode: LandMode::CheckOnly,
        },
        start(),
    ))
    .unwrap();
    assert!(
        matches!(checked, LandingResult::Checked { .. }),
        "{checked:?}"
    );
    let mut row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    let author_commit = run_git(&repo, &["rev-parse", &format!("{}^", row.branch)]);
    // A crash between recording a second amend commit and moving the branch to it.
    let tree = run_git(&repo, &["rev-parse", &format!("{}^{{tree}}", row.branch)]);
    let stray = run_git(
        &repo,
        &[
            "-c",
            "user.name=bongos",
            "-c",
            "user.email=bongos@localhost",
            "commit-tree",
            &tree,
            "-m",
            "amend: stamp edit",
        ],
    );
    row.amend_commits.push(crate::changes::AmendCommit {
        commit: stray,
        check: "stamp".to_string(),
    });
    let writer = repo.writer_or_error().unwrap();
    writer.transact(|tx| tx.set_change(&row)).unwrap();

    let ended =
        block_on(repo.drop_amendments(&writer, &mut row, &copy.path, &crate::Identity::default()))
            .unwrap();
    assert_eq!(ended, None);
    assert_eq!(head(&copy, &repo), author_commit);
    assert!(
        repo.change(&copy.change_id)
            .unwrap()
            .unwrap()
            .amend_commits
            .is_empty()
    );
}
