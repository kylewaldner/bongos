//! Letting a project's writer go to another process and taking it back: the lock moves, the
//! released writer refuses writes but still reads, and no write is lost or reordered.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::test_support::{git, location, task_with_todos, writer};
use crate::{
    OpenError, ProjectRepo, RELEASED_MESSAGE, StateApplyHooks, StateEvent, StateHandle, StateStore,
};
use bongos_work_model::TodoState;

fn repo(root: &TempDir) -> ProjectRepo {
    let location = location(root);
    let writer = writer(&location, &StateApplyHooks::new());
    ProjectRepo::new(location, git()).with_state(StateHandle::Writer(writer))
}

fn set_task(writer: &crate::StateWriter, id: &str) -> anyhow::Result<()> {
    writer.transact(|tx| tx.set_task(id, task_with_todos(&[("t1", TodoState::Open)])))
}

#[test]
fn a_released_writer_lets_another_open_take_the_lock_and_refuses_writes() {
    let root = TempDir::new().unwrap();
    let repo = repo(&root);
    let first = repo.state().unwrap().opened_writer().unwrap().clone();
    set_task(&first, "a").unwrap();
    let before = first.generation().unwrap();

    // Held: a second opener is refused.
    assert!(matches!(
        StateStore::open_writer_with_hooks(repo.location(), &StateApplyHooks::new()),
        Err(OpenError::HeldBy { .. })
    ));

    block_on(repo.release_writer(Duration::from_secs(1))).unwrap();
    assert!(first.is_released());
    assert!(repo.state().unwrap().writer().is_none());
    assert!(repo.backups_stopped());
    // Stopped only while released: the backup timer waits for a take-back instead of ending.
    assert!(repo.backups_released());
    let refused = set_task(&first, "b").unwrap_err();
    assert!(format!("{refused:#}").contains(RELEASED_MESSAGE));

    // The next writer takes the lock and continues the same store: the generation goes on.
    let next = StateStore::open_writer_with_hooks(repo.location(), &StateApplyHooks::new())
        .expect("the lock is free after the release");
    set_task(&next, "c").unwrap();
    let after = next.generation().unwrap();
    assert!(after > before, "{after} > {before}");

    // The released writer still reads, and sees the next writer's rows.
    let ids = first.read(|read| read.task_ids()).unwrap();
    assert_eq!(ids, vec!["a".to_string(), "c".to_string()]);
    assert_eq!(first.generation().unwrap(), after);
}

#[test]
fn a_pending_transaction_commits_before_the_lock_goes() {
    let root = TempDir::new().unwrap();
    let repo = repo(&root);
    let writer = repo.state().unwrap().opened_writer().unwrap().clone();
    let (entered, in_transaction) = std::sync::mpsc::channel();
    let (finish, finishing) = std::sync::mpsc::channel::<()>();
    let writing = {
        std::thread::spawn(move || {
            writer.transact(|tx| {
                entered.send(()).unwrap();
                finishing.recv().unwrap();
                tx.set_task("slow", task_with_todos(&[]))
            })
        })
    };
    in_transaction.recv().unwrap();
    let releasing = {
        let repo = repo.clone();
        std::thread::spawn(move || block_on(repo.release_writer(Duration::from_secs(1))))
    };
    // The release waits on the transaction; the transaction commits, then the lock goes.
    finish.send(()).unwrap();
    writing.join().unwrap().unwrap();
    releasing.join().unwrap().unwrap();
    let next =
        StateStore::open_writer_with_hooks(repo.location(), &StateApplyHooks::new()).unwrap();
    assert!(next.read(|read| read.task("slow")).unwrap().is_some());
}

#[test]
fn a_released_writer_relays_the_next_writers_events_and_can_be_taken_back() {
    let root = TempDir::new().unwrap();
    let repo = repo(&root);
    let writer = repo.state().unwrap().opened_writer().unwrap().clone();
    let mut events = writer.subscribe();
    let event = StateEvent::Changed {
        generation: 9,
        task_ids: vec!["a".to_string()],
        namespaces: Vec::new(),
    };
    // A held writer relays nothing: its own transactions are its events.
    writer.relay(event.clone());
    block_on(repo.release_writer(Duration::from_secs(1))).unwrap();
    writer.relay(event.clone());
    assert_eq!(block_on(events.next()), Some(event));

    // Taken back while another process holds the lock: refused, nothing changes.
    let other =
        StateStore::open_writer_with_hooks(repo.location(), &StateApplyHooks::new()).unwrap();
    assert!(matches!(
        repo.retake_writer(),
        Err(OpenError::HeldBy { .. })
    ));
    assert!(writer.is_released());
    drop(other);

    repo.retake_writer().unwrap();
    assert!(!writer.is_released());
    assert!(!repo.backups_stopped());
    assert!(!repo.backups_released());
    assert!(Arc::ptr_eq(
        repo.state().unwrap().writer().unwrap(),
        &writer
    ));
    set_task(&writer, "back").unwrap();
    assert!(matches!(
        StateStore::open_writer_with_hooks(repo.location(), &StateApplyHooks::new()),
        Err(OpenError::HeldBy { .. })
    ));
}

#[test]
fn a_waiting_open_takes_the_lock_once_it_is_released() {
    let root = TempDir::new().unwrap();
    let repo = repo(&root);
    let location = repo.location().clone();
    let (opened, waiting) = std::sync::mpsc::channel();
    let opener = std::thread::spawn(move || {
        let writer = StateStore::open_writer_waiting(&location, &StateApplyHooks::new());
        opened.send(()).ok();
        writer
    });
    // It waits while the lock is held (a generous bound only guards against a hang).
    assert!(waiting.recv_timeout(Duration::from_millis(200)).is_err());
    block_on(repo.release_writer(Duration::from_secs(1))).unwrap();
    waiting
        .recv_timeout(Duration::from_secs(60))
        .expect("the waiting open took the lock");
    let next = opener.join().unwrap().unwrap();
    set_task(&next, "host").unwrap();
}

#[test]
fn a_shutdown_stops_the_backups_for_good_and_a_release_only_until_a_take_back() {
    let root = TempDir::new().unwrap();
    let released = repo(&root);
    block_on(released.release_writer(Duration::from_secs(1))).unwrap();
    assert!(released.backups_stopped() && released.backups_released());
    released.retake_writer().unwrap();
    assert!(!released.backups_stopped() && !released.backups_released());

    // A shutdown while the writer is released (or once it moved for good) ends the timer too.
    block_on(released.release_writer(Duration::from_secs(1))).unwrap();
    released.stop_backups();
    assert!(released.backups_stopped() && !released.backups_released());

    let other = TempDir::new().unwrap();
    let shut_down = repo(&other);
    shut_down.stop_backups();
    assert!(shut_down.backups_stopped());
    assert!(!shut_down.backups_released(), "a shutdown ends the timer");
}
