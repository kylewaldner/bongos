//! `InboxState` against the real file system: other processes deliver and move items with plain
//! file system calls, which `FakeFs::watch` never reports, so most tests use `RealFs` in a
//! `TempDir` with parking allowed so real watcher events can arrive. The periodic rescan is
//! injected, so no test waits on it; the one test of it uses `FakeFs`, whose watcher reports
//! nothing, and ticks it by hand.

use super::*;
use bongos_inbox_model::format;
use chrono::{TimeZone as _, Utc};
use fs::{FakeFs, RealFs};
use gpui::{TestAppContext, UpdateGlobal as _};
use std::io::Write as _;
use tempfile::TempDir;

/// How long a test waits for something it expects before failing, well past the watcher latency
/// and a scan on a loaded machine.
pub(crate) const HANG_GUARD: Duration = Duration::from_secs(10);

pub(crate) fn item_text(summary: &str) -> String {
    format::render(&item(summary))
}

pub(crate) fn item(summary: &str) -> Item {
    Item {
        version: 1,
        kind: "note".into(),
        created: Utc.with_ymd_and_hms(2026, 11, 11, 3, 12, 45).unwrap(),
        source: "agent/test".into(),
        group: None,
        target: None,
        summary: summary.into(),
        actions: vec!["accept".into()],
        extra: Vec::new(),
        body: format!("Body of {summary}.\n"),
        body_loaded: true,
    }
}

/// The temporary directory with symbolic links resolved, as the inbox reports paths.
fn canonical(temp: &TempDir) -> PathBuf {
    std::fs::canonicalize(temp.path()).unwrap()
}

/// Writes an item outside the inbox and renames it into `dir/name`, the way a delivering process
/// that stages elsewhere on the same disk would.
fn rename_in(temp: &Path, dir: &Path, name: &str, summary: &str) -> PathBuf {
    let staging = temp.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let staged = staging.join(name);
    std::fs::write(&staged, item_text(summary)).unwrap();
    let path = dir.join(name);
    std::fs::rename(&staged, &path).unwrap();
    path
}

fn init_settings(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        InboxSettings::register(cx);
    });
    set_enabled(cx, enabled);
}

pub(crate) fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.inbox.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

/// A timer that never ticks.
fn no_rescan() -> RescanTimer {
    Arc::new(|_: &BackgroundExecutor| futures::stream::pending().boxed())
}

/// A timer the test ticks by sending on the returned channel (the first enabled period only).
fn manual_rescan() -> (RescanTimer, mpsc::UnboundedSender<()>) {
    let (sender, receiver) = mpsc::unbounded();
    let receiver = Mutex::new(Some(receiver));
    let timer: RescanTimer = Arc::new(move |_: &BackgroundExecutor| match receiver.lock().take() {
        Some(receiver) => receiver.boxed(),
        None => futures::stream::pending().boxed(),
    });
    (timer, sender)
}

fn new_inbox(
    cx: &mut TestAppContext,
    fs: Arc<dyn Fs>,
    root: &Path,
    timer: RescanTimer,
) -> Entity<InboxState> {
    cx.new(|cx| InboxState::new(fs, InboxDirs::at(root.to_path_buf()), timer, cx))
}

pub(crate) fn real_inbox(cx: &mut TestAppContext, root: &Path) -> Entity<InboxState> {
    cx.executor().allow_parking();
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.executor()));
    new_inbox(cx, fs, root, no_rescan())
}

/// Waits for `check` to hold, looking again after every `InboxEvent` (and every stats change,
/// which notifies), and fails after [`HANG_GUARD`].
pub(crate) async fn wait_for(
    cx: &mut TestAppContext,
    inbox: &Entity<InboxState>,
    what: &str,
    check: impl Fn(&InboxState) -> bool,
) {
    let (sender, mut receiver) = mpsc::unbounded::<()>();
    let _subscriptions = cx.update(|cx| {
        let on_event = sender.clone();
        [
            cx.subscribe(inbox, move |_, _: &InboxEvent, _| {
                on_event.unbounded_send(()).ok();
            }),
            cx.observe(inbox, move |_, _| {
                sender.unbounded_send(()).ok();
            }),
        ]
    });
    let mut guard = cx.executor().timer(HANG_GUARD).fuse();
    loop {
        if inbox.read_with(cx, |inbox, _| check(inbox)) {
            return;
        }
        select_biased! {
            _ = receiver.next() => {}
            _ = guard => panic!("timed out waiting for {what}"),
        }
    }
}

fn summaries(inbox: &InboxState) -> Vec<String> {
    let Some(snapshot) = inbox.snapshot() else {
        return Vec::new();
    };
    (0..snapshot.len())
        .filter_map(|index| snapshot.item(index).map(|item| item.summary.clone()))
        .collect()
}

fn has_summaries(inbox: &InboxState, expected: &[&str]) -> bool {
    summaries(inbox) == expected
}

/// Waits until `dir` has seen a change made after every earlier one: a probe file is written and
/// its watcher event awaited, so any watcher event the inbox could get for earlier changes has
/// been delivered by then.
async fn probe_events(cx: &mut TestAppContext, dir: &Path) {
    let fs = RealFs::new(None, cx.executor());
    let (events, _watcher) = fs.watch(dir, WATCH_LATENCY).await;
    let mut events = events.fuse();
    let probe = dir.join(".probe");
    std::fs::write(&probe, "probe").unwrap();
    let mut guard = cx.executor().timer(HANG_GUARD).fuse();
    loop {
        select_biased! {
            batch = events.next() => {
                if batch.is_some_and(|batch| batch.iter().any(|event| event.path == probe)) {
                    break;
                }
            }
            _ = guard => panic!("timed out waiting for the probe event"),
        }
    }
    std::fs::remove_file(&probe).unwrap();
    cx.run_until_parked();
}

#[gpui::test]
async fn an_item_renamed_into_new_reaches_the_snapshot(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "the first scan", |inbox| {
        inbox.snapshot().is_some() && inbox.stats().watches == 1
    })
    .await;
    assert!(root.join(TMP).is_dir() && root.join(ARCHIVE).is_dir());

    let path = rename_in(&base, &root.join(NEW), "1700000000.a1.host", "First");
    wait_for(cx, &inbox, "the delivered item", |inbox| {
        has_summaries(inbox, &["First"])
    })
    .await;
    inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.items()[0].0.path, path);
        assert_eq!(snapshot.unread_count(), 1);
    });
}

#[gpui::test]
async fn a_file_growing_in_tmp_never_reaches_the_snapshot(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "the first scan", |inbox| {
        inbox.snapshot().is_some()
    })
    .await;

    let text = item_text("Still writing");
    let growing = root.join(TMP).join("1700000000.b2.host");
    let mut file = std::fs::File::create(&growing).unwrap();
    for chunk in text.as_bytes().chunks(16) {
        file.write_all(chunk).unwrap();
        file.flush().unwrap();
    }
    // The pipeline is alive: a later delivery shows, and the tmp file still does not.
    rename_in(&base, &root.join(NEW), "1700000001.c3.host", "Delivered");
    wait_for(cx, &inbox, "the delivered item", |inbox| {
        has_summaries(inbox, &["Delivered"])
    })
    .await;
    file.write_all(b"more").unwrap();
    drop(file);
    inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(
            snapshot
                .items()
                .iter()
                .all(|(entry, _)| !entry.path.starts_with(root.join(TMP)))
        );
    });
}

#[gpui::test]
async fn moving_an_item_to_archive_drops_its_row(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(
        root.join(CUR).join("1700000000.d4.host:2,S"),
        item_text("Read"),
    )
    .unwrap();
    std::fs::write(
        root.join(NEW).join("1700000001.e5.host"),
        item_text("Unread"),
    )
    .unwrap();
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "both items", |inbox| {
        has_summaries(inbox, &["Read", "Unread"]) && inbox.stats().watches == 1
    })
    .await;

    std::fs::rename(
        root.join(CUR).join("1700000000.d4.host:2,S"),
        root.join(ARCHIVE).join("1700000000.d4.host:2,S"),
    )
    .unwrap();
    wait_for(cx, &inbox, "the cur item to go", |inbox| {
        has_summaries(inbox, &["Unread"])
    })
    .await;
    std::fs::rename(
        root.join(NEW).join("1700000001.e5.host"),
        root.join(ARCHIVE).join("1700000001.e5.host:2,"),
    )
    .unwrap();
    wait_for(cx, &inbox, "the new item to go", |inbox| {
        inbox.snapshot().is_some_and(|snapshot| snapshot.is_empty())
    })
    .await;
}

#[gpui::test]
async fn delivery_keeps_working_after_new_is_deleted_and_recreated(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "the first scan", |inbox| {
        inbox.snapshot().is_some() && inbox.stats().watches == 1
    })
    .await;

    std::fs::remove_dir(root.join(NEW)).unwrap();
    // The inbox may have put it back already when it saw the removal.
    std::fs::create_dir_all(root.join(NEW)).unwrap();
    wait_for(cx, &inbox, "new/ to be watched again", |inbox| {
        inbox.stats().rewatches >= 1
    })
    .await;

    rename_in(&base, &root.join(NEW), "1700000002.f6.host", "After");
    wait_for(cx, &inbox, "the item in the recreated new/", |inbox| {
        has_summaries(inbox, &["After"])
    })
    .await;
}

#[gpui::test]
async fn a_second_inbox_on_the_same_directory_reports_the_same_read_state(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(CUR).join("1700000000.a.host:2,S"), item_text("A")).unwrap();
    std::fs::write(root.join(CUR).join("1700000001.b.host:2,"), item_text("B")).unwrap();
    std::fs::write(root.join(NEW).join("1700000002.c.host"), item_text("C")).unwrap();
    init_settings(cx, true);

    let states = |inbox: &InboxState| {
        let snapshot = inbox.snapshot().unwrap();
        let read: Vec<String> = snapshot.read().map(Entry::file_name).collect();
        let unread: Vec<String> = snapshot.unread().map(Entry::file_name).collect();
        (read, unread)
    };
    let first = real_inbox(cx, &root);
    wait_for(cx, &first, "the first scan", |inbox| {
        inbox.snapshot().is_some()
    })
    .await;
    let before = first.read_with(cx, |inbox, _| states(inbox));
    assert_eq!(
        before,
        (
            vec!["1700000000.a.host:2,S".to_string()],
            vec![
                "1700000001.b.host:2,".to_string(),
                "1700000002.c.host".to_string(),
            ],
        )
    );
    drop(first);
    cx.run_until_parked();

    let second = real_inbox(cx, &root);
    wait_for(cx, &second, "the restarted scan", |inbox| {
        inbox.snapshot().is_some()
    })
    .await;
    assert_eq!(second.read_with(cx, |inbox, _| states(inbox)), before);
}

#[gpui::test]
async fn nothing_is_created_or_watched_while_disabled(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    init_settings(cx, false);
    let inbox = real_inbox(cx, &root);
    let changes = Arc::new(Mutex::new(0usize));
    let _subscription = cx.update(|cx| {
        let changes = changes.clone();
        cx.subscribe(&inbox, move |_, _: &InboxEvent, _| *changes.lock() += 1)
    });

    // A delivery elsewhere, and a refresh, while disabled.
    let elsewhere = base.join("elsewhere");
    maildir::ensure_layout(&elsewhere).unwrap();
    maildir::deliver(&elsewhere, &item("Elsewhere")).unwrap();
    inbox.update(cx, |inbox, _| inbox.refresh());
    probe_events(cx, &elsewhere.join(NEW)).await;
    assert!(!root.exists(), "a disabled inbox created its directory");
    inbox.read_with(cx, |inbox, _| {
        assert!(!inbox.is_watching());
        assert!(inbox.snapshot().is_none());
        assert_eq!(inbox.stats(), Stats::default());
    });

    set_enabled(cx, true);
    wait_for(cx, &inbox, "the first scan", |inbox| {
        inbox.snapshot().is_some() && inbox.stats().watches == 1
    })
    .await;
    rename_in(&base, &root.join(NEW), "1700000003.g7.host", "Seen");
    wait_for(cx, &inbox, "the delivered item", |inbox| {
        has_summaries(inbox, &["Seen"])
    })
    .await;

    // Turning it off drops the snapshot (one Changed) and the watcher.
    let changes_before = *changes.lock();
    set_enabled(cx, false);
    cx.run_until_parked();
    assert_eq!(*changes.lock(), changes_before + 1);
    let stats = inbox.read_with(cx, |inbox, _| {
        assert!(!inbox.is_watching());
        assert!(inbox.snapshot().is_none());
        inbox.stats()
    });
    rename_in(&base, &root.join(NEW), "1700000004.h8.host", "Unseen");
    probe_events(cx, &root.join(NEW)).await;
    assert_eq!(*changes.lock(), changes_before + 1);
    inbox.read_with(cx, |inbox, _| {
        assert!(inbox.snapshot().is_none());
        assert_eq!(inbox.stats(), stats);
    });
}

#[gpui::test]
async fn the_periodic_rescan_sees_what_the_watcher_missed(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    init_settings(cx, true);
    // `FakeFs` reports none of the real file system's changes: only the timer can see them.
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
    let (timer, ticks) = manual_rescan();
    let inbox = new_inbox(cx, fs, &root, timer);
    cx.run_until_parked();
    let scans = inbox.read_with(cx, |inbox, _| {
        assert!(inbox.snapshot().is_some_and(|snapshot| snapshot.is_empty()));
        inbox.stats().scans
    });
    assert_eq!(scans, 1);

    rename_in(&base, &root.join(NEW), "1700000005.i9.host", "Missed");
    cx.run_until_parked();
    inbox.read_with(cx, |inbox, _| {
        assert!(inbox.snapshot().unwrap().is_empty());
        assert_eq!(inbox.stats().scans, 1);
    });

    ticks.unbounded_send(()).unwrap();
    cx.run_until_parked();
    inbox.read_with(cx, |inbox, _| {
        assert!(has_summaries(inbox, &["Missed"]));
        assert_eq!(inbox.stats().scans, 2);
    });
}

#[gpui::test]
async fn overlapping_rescans_coalesce_and_an_identical_scan_emits_nothing(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(NEW).join("1700000006.j.host"), item_text("Kept")).unwrap();
    init_settings(cx, true);
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
    let (timer, ticks) = manual_rescan();
    let inbox = new_inbox(cx, fs, &root, timer);
    cx.run_until_parked();
    assert!(inbox.read_with(cx, |inbox, _| has_summaries(inbox, &["Kept"])));
    let changes = Arc::new(Mutex::new(0usize));
    let _subscription = cx.update(|cx| {
        let changes = changes.clone();
        cx.subscribe(&inbox, move |_, _: &InboxEvent, _| *changes.lock() += 1)
    });

    inbox.update(cx, |inbox, _| {
        for _ in 0..10 {
            inbox.refresh();
        }
    });
    for _ in 0..10 {
        ticks.unbounded_send(()).unwrap();
    }
    cx.run_until_parked();
    inbox.read_with(cx, |inbox, _| {
        // The first scan, then one for all twenty triggers.
        assert_eq!(inbox.stats().scans, 2);
        assert!(has_summaries(inbox, &["Kept"]));
    });
    assert_eq!(*changes.lock(), 0);
}

/// Waits for an operation to finish, failing after [`HANG_GUARD`].
pub(crate) async fn outcome_of(cx: &mut TestAppContext, task: Task<OpOutcome>) -> OpOutcome {
    let mut guard = cx.executor().timer(HANG_GUARD).fuse();
    let mut task = task.fuse();
    select_biased! {
        outcome = task => outcome,
        _ = guard => panic!("timed out waiting for an operation"),
    }
}

/// The unique and inode of the item with `summary` in the snapshot.
pub(crate) fn key_of(inbox: &InboxState, summary: &str) -> (String, u64) {
    let snapshot = inbox.snapshot().unwrap();
    let index = (0..snapshot.len())
        .find(|index| {
            snapshot
                .item(*index)
                .is_some_and(|item| item.summary == summary)
        })
        .unwrap_or_else(|| panic!("no item {summary}"));
    let entry = &snapshot.items()[index].0;
    (entry.unique.clone(), entry.inode)
}

/// The file names in `dir`, sorted, without probe files.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Counts the `InboxEvent`s the inbox emits.
fn count_changes(
    cx: &mut TestAppContext,
    inbox: &Entity<InboxState>,
) -> (Arc<Mutex<usize>>, Subscription) {
    let changes = Arc::new(Mutex::new(0usize));
    let subscription = cx.update(|cx| {
        let changes = changes.clone();
        cx.subscribe(inbox, move |_, _: &InboxEvent, _| *changes.lock() += 1)
    });
    (changes, subscription)
}

/// Collects what the inbox reports instead of showing it.
fn capture_reports(cx: &mut TestAppContext, inbox: &Entity<InboxState>) -> Arc<Mutex<Vec<String>>> {
    let reports = Arc::new(Mutex::new(Vec::new()));
    inbox.update(cx, |inbox, _| {
        let reports = reports.clone();
        inbox.set_reporter(move |text, _| reports.lock().push(text.to_string()));
    });
    reports
}

#[gpui::test]
async fn two_quick_archives_of_different_items_both_succeed(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(NEW).join("1700000010.a.host"), item_text("A")).unwrap();
    std::fs::write(root.join(CUR).join("1700000011.b.host:2,S"), item_text("B")).unwrap();
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "both items", |inbox| {
        has_summaries(inbox, &["B", "A"])
    })
    .await;
    let reports = capture_reports(cx, &inbox);

    let (a, b) = inbox.update(cx, |inbox, cx| {
        let (a_unique, a_inode) = key_of(inbox, "A");
        let (b_unique, b_inode) = key_of(inbox, "B");
        (
            inbox.archive(&a_unique, a_inode, cx),
            inbox.archive(&b_unique, b_inode, cx),
        )
    });
    let a = outcome_of(cx, a).await;
    let b = outcome_of(cx, b).await;
    assert_eq!(
        a,
        OpOutcome::Done(root.join(ARCHIVE).join("1700000010.a.host:2,"))
    );
    assert_eq!(
        b,
        OpOutcome::Done(root.join(ARCHIVE).join("1700000011.b.host:2,S"))
    );
    assert_eq!(
        names_in(&root.join(ARCHIVE)),
        ["1700000010.a.host:2,", "1700000011.b.host:2,S"]
    );
    assert!(names_in(&root.join(NEW)).is_empty() && names_in(&root.join(CUR)).is_empty());
    inbox.read_with(cx, |inbox, _| {
        assert!(inbox.snapshot().unwrap().is_empty());
        assert_eq!(inbox.stats().operations, 2);
    });
    assert!(reports.lock().is_empty());
}

#[gpui::test]
async fn two_reads_of_one_item_leave_one_read_name(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(NEW).join("1700000012.c.host"), item_text("C")).unwrap();
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "the item", |inbox| has_summaries(inbox, &["C"])).await;
    let reports = capture_reports(cx, &inbox);
    let (unique, inode) = inbox.read_with(cx, |inbox, _| key_of(inbox, "C"));

    // Two callers, each asking on its own.
    let first = inbox.update(cx, |inbox, cx| inbox.mark_read(&unique, inode, cx));
    let second = inbox.update(cx, |inbox, cx| inbox.mark_read(&unique, inode, cx));
    let read = root.join(CUR).join("1700000012.c.host:2,S");
    assert_eq!(outcome_of(cx, first).await, OpOutcome::Done(read.clone()));
    assert_eq!(outcome_of(cx, second).await, OpOutcome::Done(read));
    assert_eq!(names_in(&root.join(CUR)), ["1700000012.c.host:2,S"]);
    assert!(names_in(&root.join(NEW)).is_empty());
    inbox.read_with(cx, |inbox, _| {
        assert_eq!(inbox.snapshot().unwrap().unread_count(), 0);
    });
    assert!(reports.lock().is_empty());
}

#[gpui::test]
async fn an_operation_on_an_item_moved_by_hand_reports_and_does_nothing(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(NEW).join("1700000013.d.host"), item_text("D")).unwrap();
    init_settings(cx, true);
    // `FakeFs` reports nothing, so the snapshot still shows the item where it was.
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
    let inbox = new_inbox(cx, fs, &root, no_rescan());
    cx.run_until_parked();
    let reports = capture_reports(cx, &inbox);
    let (changes, _subscription) = count_changes(cx, &inbox);
    let (unique, inode) = inbox.read_with(cx, |inbox, _| key_of(inbox, "D"));

    let by_hand = root.join(CUR).join("1700000013.d.host:2,F");
    std::fs::rename(root.join(NEW).join("1700000013.d.host"), &by_hand).unwrap();
    let task = inbox.update(cx, |inbox, cx| inbox.archive(&unique, inode, cx));
    cx.run_until_parked();
    assert_eq!(outcome_of(cx, task).await, OpOutcome::Moved);
    assert_eq!(*reports.lock(), [MOVED_MESSAGE]);
    assert!(names_in(&root.join(ARCHIVE)).is_empty());
    assert_eq!(names_in(&root.join(CUR)), ["1700000013.d.host:2,F"]);
    // The rescan shows the item where it went.
    assert_eq!(*changes.lock(), 1);
    inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.items()[0].0.path, by_hand);
    });

    // An item removed by hand is the same.
    std::fs::remove_file(&by_hand).unwrap();
    let task = inbox.update(cx, |inbox, cx| inbox.mark_read(&unique, inode, cx));
    cx.run_until_parked();
    assert_eq!(outcome_of(cx, task).await, OpOutcome::Moved);
    assert_eq!(*reports.lock(), [MOVED_MESSAGE, MOVED_MESSAGE]);
    assert!(names_in(&root.join(CUR)).is_empty());
}

#[gpui::test]
async fn the_row_turns_read_before_the_watcher_event_which_changes_nothing(
    cx: &mut TestAppContext,
) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(root.join(NEW).join("1700000014.e.host"), item_text("E")).unwrap();
    init_settings(cx, true);
    let inbox = real_inbox(cx, &root);
    wait_for(cx, &inbox, "the item", |inbox| {
        has_summaries(inbox, &["E"]) && inbox.stats().watches == 1
    })
    .await;
    let (changes, _subscription) = count_changes(cx, &inbox);
    let (unique, inode) = inbox.read_with(cx, |inbox, _| key_of(inbox, "E"));

    let task = inbox.update(cx, |inbox, cx| inbox.mark_read(&unique, inode, cx));
    assert!(matches!(outcome_of(cx, task).await, OpOutcome::Done(_)));
    // Read as soon as the operation is done, without waiting for the watcher.
    let scans = inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.unread_count(), 0);
        assert_eq!(snapshot.read().count(), 1);
        inbox.stats().scans
    });
    assert_eq!(*changes.lock(), 1);

    // The watcher reports the link and the unlink; the rescan finds the same names.
    wait_for(cx, &inbox, "the rescan after the watcher event", |inbox| {
        inbox.stats().scans > scans
    })
    .await;
    probe_events(cx, &root.join(CUR)).await;
    assert_eq!(*changes.lock(), 1);
    inbox.read_with(cx, |inbox, _| {
        assert_eq!(inbox.snapshot().unwrap().read().count(), 1);
    });
}

pub(crate) fn item_with_target(summary: &str, target: &str) -> String {
    let mut item = item(summary);
    item.target = Some(target.to_string());
    format::render(&item)
}

#[gpui::test]
async fn targets_resolve_to_live_stale_or_unresolved(cx: &mut TestAppContext) {
    use bongos_work_model::Status;
    use bongos_work_model::fixtures;

    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("inbox");
    maildir::ensure_layout(&root).unwrap();
    let present = base.join("present.md");
    std::fs::write(&present, "here").unwrap();
    let task_dir = base.join("task-a");
    std::fs::create_dir_all(task_dir.join("runs").join("run-1")).unwrap();
    let mut tree = fixtures::tree(vec![fixtures::leaf("a", Status::Open)]);
    tree.projects[0].task.children[0].dir = task_dir;
    let task_id = fixtures::id_of(&tree, "a");
    let tree = Arc::new(tree);

    let targets = [
        ("path live", present.display().to_string()),
        ("path stale", base.join("missing.md").display().to_string()),
        ("task live", format!("task:{task_id}")),
        ("task stale", "task:no-such-task".to_string()),
        ("run live", "run:run-1".to_string()),
        ("run stale", "run:run-2".to_string()),
        ("scheme unresolved", "linear:SEQ-1".to_string()),
        ("relative unresolved", "plan.md".to_string()),
    ];
    for (index, (summary, target)) in targets.iter().enumerate() {
        std::fs::write(
            root.join(NEW).join(format!("17000001{index:02}.t.host")),
            item_with_target(summary, target),
        )
        .unwrap();
    }
    std::fs::write(
        root.join(NEW).join("1700000200.n.host"),
        item_text("no target"),
    )
    .unwrap();
    init_settings(cx, true);
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
    let inbox = new_inbox(cx, fs, &root, no_rescan());
    inbox.update(cx, |inbox, _| {
        inbox
            .resolvers_mut()
            .set_work_tree(move |_| Some(tree.clone()));
    });
    cx.run_until_parked();

    let states = inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        (0..snapshot.len())
            .map(|index| {
                (
                    snapshot.item(index).unwrap().summary.clone(),
                    snapshot.target(index),
                )
            })
            .collect::<Vec<_>>()
    });
    use TargetState::*;
    assert_eq!(
        states,
        [
            ("path live".to_string(), Some(Live)),
            ("path stale".to_string(), Some(Stale)),
            ("task live".to_string(), Some(Live)),
            ("task stale".to_string(), Some(Stale)),
            ("run live".to_string(), Some(Live)),
            ("run stale".to_string(), Some(Stale)),
            ("scheme unresolved".to_string(), Some(Unresolved)),
            ("relative unresolved".to_string(), Some(Unresolved)),
            ("no target".to_string(), None),
        ]
    );

    // The rows mark exactly the stale items, which keep open and dismiss.
    let rows = inbox.read_with(cx, |inbox, _| {
        inbox.snapshot().unwrap().rows(&Filter::All, &|_| false)
    });
    let mut stale: Vec<String> = Vec::new();
    for row in &rows {
        let items = match row {
            Row::Item(item) => std::slice::from_ref(item),
            Row::Group(group) => group.members.as_slice(),
        };
        for item in items {
            assert_eq!(item.actions, [view_model::OPEN, view_model::DISMISS]);
            if item.stale {
                stale.push(item.summary.clone());
            }
        }
    }
    stale.sort();
    assert_eq!(stale, ["path stale", "run stale", "task stale"]);
}

#[gpui::test]
async fn without_a_work_tree_task_and_run_targets_are_unresolved(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("inbox");
    maildir::ensure_layout(&root).unwrap();
    std::fs::write(
        root.join(NEW).join("1700000300.a.host"),
        item_with_target("task", "task:x"),
    )
    .unwrap();
    std::fs::write(
        root.join(NEW).join("1700000301.b.host"),
        item_with_target("run", "run:y"),
    )
    .unwrap();
    init_settings(cx, true);
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
    let inbox = new_inbox(cx, fs, &root, no_rescan());
    cx.run_until_parked();
    inbox.read_with(cx, |inbox, _| {
        let snapshot = inbox.snapshot().unwrap();
        assert_eq!(snapshot.target(0), Some(TargetState::Unresolved));
        assert_eq!(snapshot.target(1), Some(TargetState::Unresolved));
    });
}
