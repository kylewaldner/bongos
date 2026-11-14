use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use bongos_project_repo_model::{
    Author, AuthorKind, ChangeMeta, CompatibilityMark, TaskStateRecord, parse_trailers,
};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::landing::ProbePoint;
use crate::landing_queue::LandingLock;
use crate::test_support::{
    CountingHook, add_task, fixture_project, fixture_project_with_hooks, land, landed, main_commit,
    new_copy, person, reopen, request, run_git, start, verified, write_definition,
};
use crate::{
    AbandonRefused, AmendOutcome, AmendingCheck, ChangeFilter, ChangeState, LandMode, LandRequest,
    LandingEvent, LandingResult, Recovery, StateApplyHooks, StateOp,
};

/// An amending check that writes the sorted task folder names into `definitions/stamp.txt`.
struct StampCheck;

impl AmendingCheck for StampCheck {
    fn name(&self) -> &str {
        "stamp"
    }

    fn stamp_paths(&self) -> Vec<String> {
        vec!["stamp.txt".to_string()]
    }

    fn amend(&self, writable: &Path) -> BoxFuture<'static, AmendOutcome> {
        let definitions = writable.join("definitions");
        let mut slugs: Vec<String> = std::fs::read_dir(definitions.join("tasks"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        slugs.sort();
        let text = format!("{}\n", slugs.join("\n"));
        let path = definitions.join("stamp.txt");
        let outcome = if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            AmendOutcome::Unchanged
        } else {
            std::fs::write(&path, text).unwrap();
            AmendOutcome::Amended {
                message: "graph".to_string(),
            }
        };
        async move { outcome }.boxed()
    }
}

fn trailer(message: &str, key: &str) -> Option<String> {
    parse_trailers(message)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value)
}

#[test]
fn a_landing_is_one_squashed_commit_with_subject_body_and_trailers() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let old = main_commit(&repo);
    let mut author = Author::workflow("run 41 step rebalance");
    author.run = Some("41".to_string());
    author.step = Some("rebalance".to_string());
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    block_on(
        repo.git()
            .commit_all(&copy.path, "first", &crate::Identity::default()),
    )
    .unwrap();
    add_task(&copy, "beta", "b1");

    let meta = ChangeMeta {
        subject: "Add alpha and beta".to_string(),
        body: "Two new leaves.".to_string(),
        notes: "nothing started".to_string(),
        author,
        compatibility: Some(CompatibilityMark {
            safe: true,
            reason: "adds tasks only".to_string(),
        }),
    };
    let request = LandRequest {
        meta,
        mode: LandMode::Land,
    };
    let commit = landed(block_on(repo.land_now(&copy.change_id, request, start())).unwrap());

    assert_eq!(main_commit(&repo), commit);
    assert_eq!(verified(&repo).as_deref(), Some(commit.as_str()));
    // One commit on top of the old master, whatever the working copy's commits were.
    assert_eq!(
        run_git(&repo, &["rev-list", "--count", &format!("{old}..main")]),
        "1"
    );
    assert_eq!(run_git(&repo, &["rev-parse", "main^"]), old);
    let message = run_git(&repo, &["log", "-1", "--format=%B", "main"]);
    assert!(
        message.starts_with("Add alpha and beta\n\nTwo new leaves.\n\n"),
        "{message}"
    );
    assert_eq!(
        trailer(&message, "Notes").as_deref(),
        Some("nothing started")
    );
    assert_eq!(
        trailer(&message, "Bongos-Change").as_deref(),
        Some(copy.change_id.as_str())
    );
    assert_eq!(
        trailer(&message, "Bongos-Author-Kind").as_deref(),
        Some("workflow")
    );
    assert_eq!(trailer(&message, "Bongos-Run").as_deref(), Some("41"));
    assert_eq!(
        trailer(&message, "Bongos-Step").as_deref(),
        Some("rebalance")
    );
    assert_eq!(
        trailer(&message, "Bongos-Compatibility").as_deref(),
        Some("safe; adds tasks only")
    );
    assert_eq!(
        trailer(&message, "Bongos-Checks").as_deref(),
        Some("schema=pass")
    );
    assert_eq!(
        trailer(&message, "Bongos-Tree"),
        Some(run_git(&repo, &["rev-parse", "main^{tree}"]))
    );

    // The checkout fast-forwarded and is read-only again; the working copy is gone.
    let added = repo
        .location()
        .repository
        .join("definitions/tasks/beta/task.toml");
    assert!(std::fs::metadata(&added).unwrap().permissions().readonly());
    assert!(!copy.path.exists());
    assert!(!block_on(
        repo.git()
            .branch_exists(&repo.location().repository, &copy.branch)
    ));
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Landed);
    assert_eq!(row.commit.as_deref(), Some(commit.as_str()));
    // Known at once, and the waiting form resolves at once too.
    assert_eq!(
        repo.landing_result(&copy.change_id).unwrap(),
        Some(LandingResult::Landed {
            commit: commit.clone()
        })
    );
    assert_eq!(
        block_on(repo.wait_for_landing(&copy.change_id)).unwrap(),
        LandingResult::Landed { commit }
    );
}

#[test]
fn a_runs_landed_change_is_found_after_a_store_restart() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let mut author = Author::workflow("run 7 step plan");
    author.run = Some("7".to_string());
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let other = new_copy(&repo, "calm-heron-0002", &Author::agent("chat 1"));
    let commit = landed(land(&repo, &copy, &author));

    let repo = reopen(repo, &StateApplyHooks::new());
    let filter = ChangeFilter {
        run: Some("7".to_string()),
        state: Some(ChangeState::Landed),
        ..ChangeFilter::default()
    };
    let rows = repo.changes(&filter).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].change_id, copy.change_id);
    assert_eq!(rows[0].commit.as_deref(), Some(commit.as_str()));
    assert_eq!(rows[0].author.kind(), AuthorKind::Workflow);
    // The other change is still open and not listed.
    assert!(rows.iter().all(|row| row.change_id != other.change_id));
}

#[test]
fn abandon_refuses_while_the_change_holds_the_lock_and_after_it_landed() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let seen = Arc::new(Mutex::new(None));
    {
        let repo_in_probe = repo.clone();
        let seen = seen.clone();
        repo.set_probe(Arc::new(move |point, change| {
            let repo = repo_in_probe.clone();
            let seen = seen.clone();
            async move {
                if point == ProbePoint::InQueue {
                    let refused = repo.abandon_now(&change, start()).await;
                    *seen.lock() = Some(format!("{refused:?}"));
                }
                false
            }
            .boxed()
        }));
    }
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(seen.lock().as_deref(), Some("Err(Landing)"));
    match block_on(repo.abandon_now(&copy.change_id, start())) {
        Err(AbandonRefused::Landed { commit: refused }) => assert_eq!(refused, commit),
        other => panic!("expected Landed, got {other:?}"),
    }
}

#[test]
fn a_conflict_lists_the_files_with_markers_and_runs_no_check() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    write_definition(&first, "plan.md", "first plan\n");
    write_definition(&second, "plan.md", "second plan\n");
    landed(land(&repo, &first, &author));

    let result = land(&repo, &second, &author);
    assert_eq!(
        result,
        LandingResult::Conflict {
            files: vec!["definitions/plan.md".to_string()]
        }
    );
    let text = std::fs::read_to_string(second.path.join("definitions/plan.md")).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains(">>>>>>>"),
        "{text}"
    );
    assert!(repo.check_results(&second.change_id).unwrap().is_empty());
    assert_eq!(
        repo.change(&second.change_id).unwrap().unwrap().state,
        ChangeState::Requested
    );
    // Landing again with the markers still there stops at the same place.
    assert!(matches!(
        land(&repo, &second, &author),
        LandingResult::Conflict { .. }
    ));

    // Resolved by hand, it lands.
    write_definition(&second, "plan.md", "both plans\n");
    let commit = landed(land(&repo, &second, &author));
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(
        run_git(&repo, &["show", "main:definitions/plan.md"]),
        "both plans"
    );
}

/// Pauses the first change to reach the queue until another change shows `position` 1, and
/// returns what each change landed as and the amend commits the second had while it waited.
struct Race {
    first: crate::LandingResult,
    second: crate::LandingResult,
    waiting_amends: Vec<String>,
}

fn race(
    repo: &crate::ProjectRepo,
    first: &crate::WorkingCopy,
    second: &crate::WorkingCopy,
    author: &Author,
) -> Race {
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
    let land_first = repo.land_now(&first.change_id, request("first", author), start());
    let land_second = async {
        paused_rx.await.unwrap();
        repo.land_now(&second.change_id, request("second", author), start())
            .await
    };
    let watch = async {
        let mut amends = Vec::new();
        while let Some((change, event)) = events.next().await {
            if change == second_id && event == (LandingEvent::Queued { position: 1 }) {
                amends = repo
                    .change(&second_id)
                    .unwrap()
                    .unwrap()
                    .amend_commits
                    .into_iter()
                    .map(|amend| amend.commit)
                    .collect();
                open_tx.send(()).unwrap();
                break;
            }
        }
        amends
    };
    let (first, second, waiting_amends) =
        block_on(async { futures::join!(land_first, land_second, watch) });
    Race {
        first: first.unwrap(),
        second: second.unwrap(),
        waiting_amends,
    }
}

#[test]
fn two_simultaneous_landings_serialize_and_the_second_lands_rebased() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");

    let race = race(&repo, &first, &second, &author);
    let first_commit = landed(race.first);
    let second_commit = landed(race.second);
    assert_eq!(main_commit(&repo), second_commit);
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{second_commit}^")]),
        first_commit
    );
    let files = run_git(&repo, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(files.contains("definitions/tasks/alpha/task.toml"));
    assert!(files.contains("definitions/tasks/beta/task.toml"));
    // The second change's checks ran again on the rebased tree.
    let results = repo.check_results(&second.change_id).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].tree,
        run_git(&repo, &["rev-parse", "main^{tree}"])
    );
    assert!(repo.landing_holder().is_none());
    assert!(
        repo.state()
            .unwrap()
            .read(|read| read.landing_queue())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn two_changes_amending_one_stamp_file_land_one_after_the_other_without_a_conflict() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_amending_check(Arc::new(StampCheck));
    let author = Author::agent("chat 1");
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");

    let race = race(&repo, &first, &second, &author);
    landed(race.first);
    let commit = landed(race.second);
    assert_eq!(main_commit(&repo), commit);
    // The second change was amended before it queued, dropped and amended again after the
    // first landed: the stamp names both tasks, and no conflict stopped it.
    assert_eq!(race.waiting_amends.len(), 1);
    let row = repo.change(&second.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_ne!(row.amend_commits[0].commit, race.waiting_amends[0]);
    assert_eq!(row.amend_commits[0].check, "stamp");
    assert_eq!(
        run_git(&repo, &["show", "main:definitions/stamp.txt"]),
        "alpha\nbackfill\nbeta\nreport"
    );
}

#[test]
fn a_hooks_kv_write_commits_with_the_landing_and_sees_the_redefined_task() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting-hook", false);
    let repo = fixture_project_with_hooks(&root, &CountingHook::registry(&hook));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &crate::test_support::definition_toml(
            "l1",
            "Backfill everything",
            "other",
            &[("t1", "implement"), ("t2", "review"), ("t3", "check")],
        ),
    );
    let generation_before = repo
        .state()
        .unwrap()
        .read(|read| read.generation())
        .unwrap();
    let commit = landed(land(&repo, &copy, &author));

    assert_eq!(hook.call_count(), 1);
    assert_eq!(hook.calls.lock()[0], vec!["l1".to_string()]);
    let (value, landed_in, verified_in) = repo
        .state()
        .unwrap()
        .read(|read| {
            Ok((
                read.kv("test-hook").get("redefined")?,
                read.change(&copy.change_id)?.unwrap().state,
                read.verified_commit()?,
            ))
        })
        .unwrap();
    assert_eq!(
        value,
        Some(toml::Value::Array(vec![toml::Value::String(
            "l1".to_string()
        )]))
    );
    assert_eq!(landed_in, ChangeState::Landed);
    assert_eq!(verified_in.as_deref(), Some(commit.as_str()));
    assert!(
        repo.state()
            .unwrap()
            .read(|read| read.generation())
            .unwrap()
            > generation_before
    );
}

#[test]
fn a_failing_hook_leaves_verified_and_main_and_ends_as_no_verdict_naming_it() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("failing-hook", true);
    let repo = fixture_project_with_hooks(&root, &CountingHook::registry(&hook));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let main_before = main_commit(&repo);

    let result = land(&repo, &copy, &author);
    match &result {
        LandingResult::NoVerdict { reason } => {
            assert!(
                reason.contains("state hook failing-hook failed"),
                "{reason}"
            )
        }
        other => panic!("expected no verdict, got {other:?}"),
    }
    assert_eq!(main_commit(&repo), main_before);
    assert_eq!(verified(&repo), None);
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Requested);
    assert!(repo.landing_holder().is_none());
    assert!(copy.path.is_dir());
    let in_flight = repo
        .state()
        .unwrap()
        .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
        .unwrap();
    assert_eq!(in_flight, None);
}

/// Lands `copy` until the probe point `crash`, where it stops as a killed process would.
fn land_until_crash(
    repo: &crate::ProjectRepo,
    copy: &crate::WorkingCopy,
    author: &Author,
    crash: ProbePoint,
) {
    repo.set_probe(Arc::new(move |point, _| {
        async move { point == crash }.boxed()
    }));
    let error =
        block_on(repo.land_now(&copy.change_id, request("crash", author), start())).unwrap_err();
    assert!(format!("{error}").contains(crate::landing::SIMULATED_CRASH));
}

#[test]
fn a_crash_before_the_ref_move_is_completed_at_startup_without_the_hook() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting-hook", false);
    let hooks = CountingHook::registry(&hook);
    let repo = fixture_project_with_hooks(&root, &hooks);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);

    land_until_crash(&repo, &copy, &author, ProbePoint::AfterStateTransaction);
    let recorded = verified(&repo).unwrap();
    assert_eq!(main_commit(&repo), old);
    assert_eq!(hook.call_count(), 1);

    let repo = reopen(repo, &hooks);
    assert_eq!(
        block_on(repo.recover_landing()).unwrap(),
        Recovery::Completed {
            commit: recorded.clone()
        }
    );
    assert_eq!(main_commit(&repo), recorded);
    assert_eq!(hook.call_count(), 1);
    assert!(
        repo.location()
            .repository
            .join("definitions/tasks/alpha/task.toml")
            .is_file()
    );
    assert!(
        repo.state()
            .unwrap()
            .read(|read| read.landing_queue())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.state()
            .unwrap()
            .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
            .unwrap(),
        None
    );
    assert_eq!(
        repo.landing_result(&copy.change_id).unwrap(),
        Some(LandingResult::Landed { commit: recorded })
    );
    // Recovering again finds nothing to do.
    assert_eq!(block_on(repo.recover_landing()).unwrap(), Recovery::Nothing);
}

#[test]
fn a_crash_after_the_ref_move_only_finishes_the_checkout_and_clears_the_marker() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");

    land_until_crash(&repo, &copy, &author, ProbePoint::AfterRefMove);
    let recorded = verified(&repo).unwrap();
    assert_eq!(main_commit(&repo), recorded);
    let checkout = repo
        .location()
        .repository
        .join("definitions/tasks/alpha/task.toml");
    assert!(!checkout.exists());

    let repo = reopen(repo, &StateApplyHooks::new());
    assert_eq!(
        block_on(repo.recover_landing()).unwrap(),
        Recovery::Completed {
            commit: recorded.clone()
        }
    );
    assert_eq!(main_commit(&repo), recorded);
    assert!(checkout.is_file());
    assert!(
        std::fs::metadata(&checkout)
            .unwrap()
            .permissions()
            .readonly()
    );
    assert!(repo.split_master().unwrap().is_none());
}

#[test]
fn state_commits_never_reach_main() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let writer = repo.state().unwrap().writer().unwrap().clone();
    writer
        .transact(|tx| {
            tx.set_task("l2", TaskStateRecord::default())?;
            tx.stage(
                &copy.change_id,
                &[StateOp::Set {
                    task: "l2".to_string(),
                    field: "status".to_string(),
                    expected: Some(toml::Value::String("open".to_string())),
                    value: Some(toml::Value::String("done".to_string())),
                }],
            )
        })
        .unwrap();
    landed(land(&repo, &copy, &author));

    // The staged state was applied with the landing and its row dropped.
    let (status, staged) = writer
        .read(|read| {
            Ok((
                read.task("l2")?.unwrap().status,
                read.staged(&copy.change_id)?,
            ))
        })
        .unwrap();
    assert_eq!(status, bongos_work_model::Status::Done);
    assert_eq!(staged, None);
    // Nothing of state is in any commit of master.
    let files = run_git(&repo, &["log", "--format=", "--name-only", "main"]);
    for file in files.lines().filter(|line| !line.is_empty()) {
        assert!(
            file == ".gitattributes" || file.starts_with("definitions/"),
            "{file} reached master"
        );
    }
    assert!(!files.contains("state.db"));
    assert!(!run_git(&repo, &["branch", "--list"]).contains("bongos/state"));
}

#[test]
fn master_is_pushed_to_the_remote_after_landing() {
    let root = TempDir::new().unwrap();
    let mut repo = fixture_project(&root);
    let remote = root.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    block_on(
        repo.git()
            .run(&remote, &["init", "-q", "--bare", "-b", "main"]),
    )
    .unwrap();
    repo.location.remote = Some(remote.to_string_lossy().into_owned());
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(
        block_on(repo.git().rev_parse(&remote, "main")).unwrap(),
        commit
    );
    assert_eq!(repo.push_failure().unwrap(), None);

    // A remote that refuses is recorded, and the landing still lands.
    repo.location.remote = Some(
        root.path()
            .join("missing.git")
            .to_string_lossy()
            .into_owned(),
    );
    let copy = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&copy, "beta", "b1");
    landed(land(&repo, &copy, &author));
    assert!(repo.push_failure().unwrap().is_some());
}

#[test]
fn check_only_runs_the_checks_without_queueing_or_landing() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(&copy, "junk.txt", "not a definition\n");
    let before = main_commit(&repo);
    let result = block_on(repo.land_now(
        &copy.change_id,
        LandRequest {
            meta: ChangeMeta::new("check", author.clone()),
            mode: LandMode::CheckOnly,
        },
        start(),
    ))
    .unwrap();
    let LandingResult::Checked { checks, .. } = result else {
        panic!("expected check results, got {result:?}");
    };
    assert_eq!(checks.len(), 1);
    assert!(
        matches!(&checks[0].outcome, crate::CheckOutcome::Fail { message } if message.contains("junk.txt"))
    );
    assert_eq!(main_commit(&repo), before);
    assert_eq!(
        repo.change(&copy.change_id).unwrap().unwrap().state,
        ChangeState::Open
    );
    // Landing it is refused by the same check.
    assert!(matches!(
        land(&repo, &copy, &author),
        LandingResult::Refused { reasons } if reasons[0].contains("junk.txt")
    ));
}

const LOCK_HOLDER_DIR: &str = "BONGOS_LANDING_TEST_LOCK_DIR";
const LOCK_HOLDER_MARKER: &str = "landing-lock-held-by-child";
const LOCK_HOLDER_TEST: &str = "landing_tests::hold_the_landing_lock_for_another_test";

/// Not a test of its own: `a_killed_queue_holder_releases_the_lock` runs the test binary again
/// with only this test, and this holds the landing lock until the child is killed.
#[test]
fn hold_the_landing_lock_for_another_test() {
    let Some(state) = std::env::var_os(LOCK_HOLDER_DIR) else {
        return;
    };
    let path = PathBuf::from(state).join(bongos_project_repo_model::layout::LANDING_LOCK_FILE);
    let Ok(_lock) = LandingLock::try_take(&path) else {
        panic!("could not take the landing lock");
    };
    println!("{LOCK_HOLDER_MARKER}");
    loop {
        std::thread::park();
    }
}

#[test]
#[allow(clippy::disallowed_methods)]
fn a_killed_queue_holder_releases_the_lock() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            LOCK_HOLDER_TEST,
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LOCK_HOLDER_DIR, &repo.location().state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let holding = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains(LOCK_HOLDER_MARKER));
    assert!(holding, "the holder exited before it held the lock");

    match land(&repo, &copy, &author) {
        LandingResult::NoVerdict { reason } => {
            assert!(reason.contains(&format!("pid {}", child.id())), "{reason}")
        }
        other => panic!("expected no verdict, got {other:?}"),
    }
    child.kill().unwrap();
    child.wait().unwrap();
    landed(land(&repo, &copy, &author));
}

#[test]
fn a_change_abandoned_while_it_took_the_lock_never_lands() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);
    {
        // An abandon whose transaction committed just before the landing took the lock.
        let repo_in_probe = repo.clone();
        repo.set_probe(Arc::new(move |point, change| {
            let repo = repo_in_probe.clone();
            async move {
                if point == ProbePoint::InQueue {
                    let mut row = repo.change(&change).unwrap().unwrap();
                    row.state = ChangeState::Abandoned;
                    repo.writer_or_error()
                        .unwrap()
                        .transact(|tx| tx.set_change(&row))
                        .unwrap();
                }
                false
            }
            .boxed()
        }));
    }
    assert_eq!(land(&repo, &copy, &author), LandingResult::Abandoned);
    assert_eq!(main_commit(&repo), old);
    assert_eq!(verified(&repo), None);
    assert_eq!(
        repo.change(&copy.change_id).unwrap().unwrap().state,
        ChangeState::Abandoned
    );
    assert!(repo.landing_holder().is_none());
    assert!(
        repo.state()
            .unwrap()
            .read(|read| read.landing_queue())
            .unwrap()
            .is_empty()
    );
    // The abandon that marked the row first still removes what is left.
    block_on(repo.abandon_now(&copy.change_id, start())).unwrap();
    assert!(!copy.path.exists());
}

#[test]
fn a_second_land_of_a_change_being_landed_waits_and_the_first_lands() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let seen = Arc::new(Mutex::new(None));
    {
        let repo_in_probe = repo.clone();
        let seen = seen.clone();
        repo.set_probe(Arc::new(move |point, change| {
            let repo = repo_in_probe.clone();
            let seen = seen.clone();
            async move {
                if point == ProbePoint::InQueue {
                    let author = Author::agent("chat 1");
                    let second = repo.land_now(&change, request("again", &author), start());
                    *seen.lock() = Some(second.await.unwrap());
                }
                false
            }
            .boxed()
        }));
    }
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(
        seen.lock().clone(),
        Some(LandingResult::NoVerdict {
            reason: crate::ALREADY_LANDING.to_string()
        })
    );
    assert_eq!(main_commit(&repo), commit);
    // Once the first is done, the change can be landed (here: is already landed) again.
    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::Landed { commit }
    );
}

/// An amending check that changes nothing on its first call and gives no verdict after.
struct NoVerdictTheSecondTime(Mutex<usize>);

impl AmendingCheck for NoVerdictTheSecondTime {
    fn name(&self) -> &str {
        "flaky"
    }

    fn amend(&self, _writable: &Path) -> BoxFuture<'static, AmendOutcome> {
        let mut calls = self.0.lock();
        *calls += 1;
        let outcome = if *calls == 1 {
            AmendOutcome::Unchanged
        } else {
            AmendOutcome::NoVerdict {
                reason: "not sure".to_string(),
            }
        };
        async move { outcome }.boxed()
    }
}

#[test]
fn edits_typed_while_a_landing_waits_for_a_split_master_are_never_discarded() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_amending_check(Arc::new(NoVerdictTheSecondTime(Mutex::new(0))));
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let typed = copy.path.join("definitions/documents/mine.md");
    {
        // In the queue, the person types into the working copy and master splits.
        let repo_in_probe = repo.clone();
        let typed = typed.clone();
        repo.set_probe(Arc::new(move |point, change| {
            let repo = repo_in_probe.clone();
            let typed = typed.clone();
            async move {
                if point == ProbePoint::InQueue && !typed.exists() {
                    std::fs::create_dir_all(typed.parent().unwrap()).unwrap();
                    std::fs::write(&typed, "my notes\n").unwrap();
                    let main = repo
                        .git()
                        .rev_parse(&repo.location().repository, "main")
                        .await
                        .unwrap();
                    let writer = repo.writer_or_error().unwrap();
                    repo.record_split(&writer, &change, &main, &main, &main)
                        .await
                        .unwrap();
                }
                false
            }
            .boxed()
        }));
    }
    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::NoVerdict {
            reason: crate::UNSAVED_EDITS.to_string()
        }
    );
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), "my notes\n");
}

/// An amending check that writes the stamp, and while it runs the person types `typed` into the
/// working copy.
struct TypingCheck {
    typed: PathBuf,
}

impl AmendingCheck for TypingCheck {
    fn name(&self) -> &str {
        "stamp"
    }

    fn stamp_paths(&self) -> Vec<String> {
        vec!["stamp.txt".to_string()]
    }

    fn amend(&self, writable: &Path) -> BoxFuture<'static, AmendOutcome> {
        if !self.typed.exists() {
            std::fs::create_dir_all(self.typed.parent().unwrap()).unwrap();
            std::fs::write(&self.typed, "mine\n").unwrap();
        }
        StampCheck.amend(writable)
    }
}

fn land_typing_into(file: &str) {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let typed = copy.path.join("definitions").join(file);
    repo.add_amending_check(Arc::new(TypingCheck {
        typed: typed.clone(),
    }));
    let old = main_commit(&repo);
    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::NoVerdict {
            reason: crate::UNSAVED_EDITS.to_string()
        },
        "{file}"
    );
    assert_eq!(main_commit(&repo), old);
    // What the person typed is still there, uncommitted.
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), "mine\n", "{file}");
    let status = block_on(repo.git().run(
        &copy.path,
        &["status", "--porcelain", "--untracked-files=all"],
    ))
    .unwrap();
    assert!(status.contains(file), "{file}: {status}");
    // No scratch copy is left behind.
    assert!(
        !repo
            .location()
            .state
            .join("amending/amber-otter-0001")
            .exists()
    );
}

#[test]
fn edits_typed_while_an_amending_check_runs_are_neither_committed_nor_discarded() {
    // A file the check does not write: the amend commit goes in beside it.
    land_typing_into("documents/mine.md");
    // The file the check writes: the working copy keeps the person's version.
    land_typing_into("stamp.txt");
}

#[test]
fn a_ref_move_held_up_by_a_git_lock_lands_once_the_lock_is_gone() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let lock = repo.location().repository.join(".git/refs/heads/main.lock");
    {
        repo.set_probe(Arc::new(move |point, _| {
            if point == ProbePoint::BeforeRefMove {
                std::fs::write(&lock, "").unwrap();
                let lock = lock.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    std::fs::remove_file(&lock).unwrap();
                });
            }
            async { false }.boxed()
        }));
    }
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(main_commit(&repo), commit);
    assert!(repo.landing_holder().is_none());
    assert_eq!(
        repo.state()
            .unwrap()
            .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
            .unwrap(),
        None
    );
}

#[test]
fn a_ref_move_that_never_got_through_is_finished_by_the_next_master_check() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);
    let lock = repo.location().repository.join(".git/refs/heads/main.lock");
    {
        let lock = lock.clone();
        let first = copy.change_id.clone();
        repo.set_probe(Arc::new(move |point, change| {
            if point == ProbePoint::BeforeRefMove && change == first {
                std::fs::write(&lock, "").unwrap();
            }
            async { false }.boxed()
        }));
    }
    // The state says landed; the lock is let go and the marker stays.
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(main_commit(&repo), old);
    assert!(repo.landing_holder().is_none());
    let marker = || {
        repo.state()
            .unwrap()
            .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
            .unwrap()
    };
    assert!(marker().is_some());

    // Once git lets go of `main`, the next master check (the ref watch, or the next landing)
    // finishes the move.
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(
        block_on(repo.check_master()).unwrap(),
        crate::MasterStatus::Clean
    );
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(marker(), None);
    let next = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&next, "beta", "b1");
    let next_commit = landed(land(&repo, &next, &author));
    assert_eq!(main_commit(&repo), next_commit);
}

#[test]
fn a_marker_left_after_the_ref_moved_is_cleared_by_the_next_master_check() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);
    let commit = landed(land(&repo, &copy, &author));
    // As when the transaction that clears the marker failed.
    let in_flight = toml::to_string(&crate::landing::InFlight {
        change_id: copy.change_id,
        commit: commit.clone(),
        old,
    })
    .unwrap();
    let writer = repo.writer_or_error().unwrap();
    writer
        .transact(|tx| tx.set_meta(crate::landing::LANDING_IN_FLIGHT_KEY, &in_flight))
        .unwrap();
    assert_eq!(
        block_on(repo.check_master()).unwrap(),
        crate::MasterStatus::Clean
    );
    assert_eq!(
        writer
            .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
            .unwrap(),
        None
    );
    assert_eq!(main_commit(&repo), commit);
}

#[test]
fn an_abandon_just_before_the_state_transaction_is_refused_and_one_already_committed_wins() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);
    let seen = Arc::new(Mutex::new(None));
    {
        let repo_in_probe = repo.clone();
        let seen = seen.clone();
        repo.set_probe(Arc::new(move |point, change| {
            let repo = repo_in_probe.clone();
            let seen = seen.clone();
            async move {
                if point == ProbePoint::BeforeStateTransaction {
                    // The change holds the lock: `abandon` refuses.
                    let refused = repo.abandon_now(&change, start()).await;
                    *seen.lock() = Some(format!("{refused:?}"));
                    // An abandon that committed anyway (from the row it read before the lock)
                    // is seen by the transaction, which reads the row again.
                    let mut row = repo.change(&change).unwrap().unwrap();
                    row.state = ChangeState::Abandoned;
                    repo.writer_or_error()
                        .unwrap()
                        .transact(|tx| tx.set_change(&row))
                        .unwrap();
                }
                false
            }
            .boxed()
        }));
    }
    assert_eq!(land(&repo, &copy, &author), LandingResult::Abandoned);
    assert_eq!(seen.lock().as_deref(), Some("Err(Landing)"));
    assert_eq!(main_commit(&repo), old);
    assert_eq!(verified(&repo), None);
    assert!(repo.landing_holder().is_none());
    assert_eq!(
        repo.state()
            .unwrap()
            .read(|read| read.meta(crate::landing::LANDING_IN_FLIGHT_KEY))
            .unwrap(),
        None
    );
}
