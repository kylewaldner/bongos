//! Races around the landing lock: the completion of a landing left in flight, abandoning a
//! queued change, and edits typed while the checks run.

use std::sync::Arc;

use bongos_project_repo_model::Author;
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::oneshot;
use futures::executor::block_on;
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::check_fakes::{FakeCheck, ManualTimer};
use crate::landing::{InFlight, LANDING_IN_FLIGHT_KEY, ProbePoint};
use crate::test_support::{
    add_task, fixture_project, land, landed, main_commit, new_copy, person, request, run_git, start,
};
use crate::{CheckPhase, LandingEvent, LandingResult, MasterStatus};

fn marker(repo: &crate::ProjectRepo) -> Option<String> {
    repo.state()
        .unwrap()
        .read(|read| read.meta(LANDING_IN_FLIGHT_KEY))
        .unwrap()
}

#[test]
fn a_landing_left_in_flight_is_completed_only_by_the_holder_of_the_lock() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let first = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&first, "alpha", "a1");
    let old = main_commit(&repo);
    let lock = repo.location().repository.join(".git/refs/heads/main.lock");
    let first_id = first.change_id.clone();
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&second, "beta", "b1");
    let second_id = second.change_id.clone();
    let seen: Arc<Mutex<Option<(MasterStatus, String)>>> = Arc::new(Mutex::new(None));
    {
        let repo_in_probe = repo.clone();
        let seen = seen.clone();
        repo.set_probe(Arc::new(move |point, change| {
            if point == ProbePoint::BeforeRefMove && change == first_id {
                std::fs::write(&lock, "").unwrap();
            }
            let repo = repo_in_probe.clone();
            let seen = seen.clone();
            let lock = lock.clone();
            let holds = point == ProbePoint::InQueue && change == second_id;
            async move {
                if holds {
                    // git lets go of `main`; the second change holds the landing lock. The ref
                    // watch's master check must leave the first landing to the holder.
                    std::fs::remove_file(&lock).unwrap();
                    let status = repo.check_master().await.unwrap();
                    let main = repo
                        .git()
                        .rev_parse(&repo.location().repository, "main")
                        .await
                        .unwrap();
                    *seen.lock() = Some((status, main));
                }
                false
            }
            .boxed()
        }));
    }
    // The first landing's ref move never gets through; its marker stays.
    let first_commit = landed(land(&repo, &first, &author));
    assert_eq!(main_commit(&repo), old);
    assert!(marker(&repo).is_some());

    let second_commit = landed(land(&repo, &second, &author));
    assert_eq!(seen.lock().clone(), Some((MasterStatus::InFlight, old)));
    // The holder's queue step completed the first landing, then landed on top of it.
    assert_eq!(main_commit(&repo), second_commit);
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{second_commit}^")]),
        first_commit
    );
    assert_eq!(marker(&repo), None);
}

#[test]
fn finishing_a_checkout_clears_only_its_own_marker() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let writer = repo.writer_or_error().unwrap();
    let main = main_commit(&repo);
    let later = InFlight {
        change_id: "b".to_string(),
        commit: main.clone(),
        old: main,
    };
    let earlier = InFlight {
        change_id: "a".to_string(),
        ..later.clone()
    };
    let text = toml::to_string(&later).unwrap();
    writer
        .transact(|tx| tx.set_meta(LANDING_IN_FLIGHT_KEY, &text))
        .unwrap();
    block_on(repo.finish_checkout(&writer, &earlier)).unwrap();
    assert_eq!(marker(&repo), Some(text));
    block_on(repo.finish_checkout(&writer, &later)).unwrap();
    assert_eq!(marker(&repo), None);
}

#[test]
fn a_change_only_waiting_in_the_queue_can_be_abandoned() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = Author::agent("chat 1");
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");
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
    let watch = async {
        let mut refused = None;
        while let Some((change, event)) = events.next().await {
            if change == second_id && event == (LandingEvent::Queued { position: 1 }) {
                // The first holds the lock: abandoning it is refused, the queued second is not.
                refused = Some(repo.abandon_now(&first.change_id, start()).await);
                repo.abandon_now(&second_id, start()).await.unwrap();
                open_tx.send(()).unwrap();
                break;
            }
        }
        refused
    };
    let (first_result, second_result, refused) =
        block_on(async { futures::join!(land_first, land_second, watch) });
    assert!(
        matches!(refused, Some(Err(crate::AbandonRefused::Landing))),
        "{refused:?}"
    );
    landed(first_result.unwrap());
    assert_eq!(second_result.unwrap(), LandingResult::Abandoned);
    assert!(!second.path.exists());
    assert!(!run_git(&repo, &["ls-tree", "-r", "--name-only", "main"]).contains("tasks/beta"));
}

#[test]
fn edits_typed_while_the_checks_run_do_not_land_and_are_kept() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let typing = Arc::new(FakeCheck::new("typing", CheckPhase::BeforeQueue));
    repo.add_landing_check(typing.clone());
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let typed = copy.path.join("definitions/documents/typed.md");
    *typing.typed.lock() = Some(typed.clone());

    let commit = landed(land(&repo, &copy, &author));
    assert!(!run_git(&repo, &["ls-tree", "-r", "--name-only", &commit]).contains("typed.md"));
    // The working copy stays with what was typed, uncommitted.
    assert_eq!(
        std::fs::read_to_string(&typed).unwrap(),
        "typed while checking\n"
    );
}

/// A change whose in-queue check never ends, holding the lock, with a timer the test fires.
fn hung_in_queue(repo: &crate::ProjectRepo) -> (ManualTimer, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let hung = Arc::new(FakeCheck::new("hung", CheckPhase::InQueue));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *hung.gate.lock() = Some((started_tx, release_rx));
    repo.add_landing_check(hung);
    let timer = ManualTimer::default();
    repo.set_timer(timer.timer());
    (timer, started_rx, release_tx)
}

#[test]
fn a_hung_in_queue_check_gives_no_verdict_at_the_queue_limit_and_the_next_change_lands() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let (timer, started, _release) = hung_in_queue(&repo);
    let author = Author::agent("chat 1");
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");
    let mut events = repo.landing_events();
    let second_id = second.change_id.clone();
    let land_first = repo.land_now(&first.change_id, request("first", &author), start());
    let land_second = async {
        started.await.unwrap();
        repo.land_now(&second.change_id, request("second", &author), start())
            .await
    };
    let watch = async {
        while let Some((change, event)) = events.next().await {
            if change == second_id && event == (LandingEvent::Queued { position: 1 }) {
                // The second waits for the lock the first holds while its check hangs; then
                // the queue limit is reached.
                assert_eq!(repo.landing_holder(), Some(first.change_id.clone()));
                assert_eq!(timer.fire(), 1);
                break;
            }
        }
    };
    let (first_result, second_result, ()) =
        block_on(async { futures::join!(land_first, land_second, watch) });
    let LandingResult::NoVerdict { reason } = first_result.unwrap() else {
        panic!("expected no verdict");
    };
    assert!(reason.contains("queue limit"), "{reason}");
    let commit = landed(second_result.unwrap());
    assert_eq!(main_commit(&repo), commit);
    let files = run_git(&repo, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(files.contains("tasks/beta") && !files.contains("tasks/alpha"));
    assert_eq!(repo.landing_holder(), None);
    // The first still waits to be landed again.
    assert_eq!(
        repo.change(&first.change_id).unwrap().unwrap().state,
        crate::changes::ChangeState::Requested
    );
}

#[test]
fn a_change_whose_in_queue_check_hangs_can_be_abandoned_once_the_queue_limit_ends_it() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let (timer, started, _release) = hung_in_queue(&repo);
    let author = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(&repo);
    let land_it = repo.land_now(&copy.change_id, request("first", &author), start());
    let control = async {
        started.await.unwrap();
        // It holds the lock while the check hangs, so abandoning it is refused.
        let refused = repo.abandon_now(&copy.change_id, start()).await;
        timer.fire();
        refused
    };
    let (result, refused) = block_on(async { futures::join!(land_it, control) });
    assert!(
        matches!(refused, Err(crate::AbandonRefused::Landing)),
        "{refused:?}"
    );
    assert!(
        matches!(result.unwrap(), LandingResult::NoVerdict { .. }),
        "expected no verdict"
    );
    assert_eq!(repo.landing_holder(), None);
    block_on(repo.abandon_now(&copy.change_id, start())).unwrap();
    assert_eq!(
        block_on(repo.wait_for_landing(&copy.change_id)).unwrap(),
        LandingResult::Abandoned
    );
    assert!(!copy.path.exists());
    assert_eq!(main_commit(&repo), old);
}
