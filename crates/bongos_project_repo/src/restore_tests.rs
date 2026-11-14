use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::{Status, TodoState};
use chrono::{DateTime, Duration, TimeZone as _, Utc};
use futures::executor::block_on;
use parking_lot::Mutex;
use tempfile::TempDir;
use util::rel_path::{RelPath, RelPathBuf};

use crate::backup::{SnapshotOutcome, SnapshotReason};
use crate::host_boundary::RESTORE_NEEDS_A_WINDOW;
use crate::peer::{ClientKind, PeerIdentity};
use crate::restore::{check_format, replaced_time, snapshot_path_problem};
use crate::snapshot::{self, Secrets};
use crate::snapshot_tests::{change_state, start, test_options, writer};
use crate::state_branch;
use crate::test_support::{fixture_project_with_hooks, task_with_todos, todo};
use crate::{
    ActivitySource, HostBoundary, ProjectRepo, ProjectValueOverrides, REPLACED_DIR,
    RUN_RECORD_MISSING, RepoCommand, RepoReply, RestoreOutcome, RestoreSource, SourceKind,
    StateApplyChange, StateApplyHook, StateApplyHooks, StateExporter, StateRead, StateTx,
    WITHHELD_SECRET, WORKTREE_MISSING,
};

const STATE_REF: &str = "refs/heads/bongos/state";
const LEDGER: &str = "ledger";
const LEDGER_FILE: &str = "ledger/entries.toml";

/// The exporter of the `ledger` namespace: every entry in `ledger/entries.toml`. Its import
/// replaces the namespace, and refuses a file holding `refuse = true`.
struct Ledger;

impl StateExporter for Ledger {
    fn namespace(&self) -> &str {
        LEDGER
    }

    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        let table: toml::Table = read.kv(LEDGER).scan("")?.into_iter().collect();
        Ok(vec![(
            RelPath::unix(LEDGER_FILE).unwrap().to_rel_path_buf(),
            toml::to_string(&table)?,
        )])
    }

    fn import(&self, tx: &mut StateTx, files: &[(RelPathBuf, String)]) -> Result<()> {
        let mut ledger = tx.kv(LEDGER);
        ledger.clear()?;
        for (path, text) in files {
            if path.as_unix_str() != LEDGER_FILE {
                bail!("unexpected file {}", path.as_unix_str());
            }
            let table: toml::Table = toml::from_str(text)?;
            if table.get("refuse") == Some(&toml::Value::Boolean(true)) {
                bail!("the ledger refuses this file");
            }
            for (key, value) in table {
                ledger.put(&key, value)?;
            }
        }
        Ok(())
    }
}

/// A state apply hook that would keep a count of every transaction in the ledger.
struct CountingLedgerHook {
    calls: AtomicUsize,
}

impl StateApplyHook for CountingLedgerHook {
    fn name(&self) -> &str {
        "ledger counter"
    }

    fn wants(&self, _change: &StateApplyChange) -> bool {
        true
    }

    fn apply(&self, tx: &mut StateTx, _change: &StateApplyChange) -> Result<()> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        tx.kv(LEDGER)
            .put("hook-count", toml::Value::Integer(calls as i64))
    }
}

/// Work that runs until it is asked to stop at a job boundary.
struct FakeActivity {
    running: Mutex<Vec<String>>,
}

impl ActivitySource for FakeActivity {
    fn name(&self) -> &str {
        "lanes"
    }

    fn activity(&self, _project_id: &str) -> Vec<String> {
        self.running.lock().clone()
    }

    fn stop_at_job_boundary(&self, _project_id: &str) -> Result<()> {
        self.running.lock().clear();
        Ok(())
    }
}

fn project_with(root: &TempDir, hooks: &StateApplyHooks) -> ProjectRepo {
    let repo = fixture_project_with_hooks(root, hooks);
    repo.set_backup_options(test_options());
    repo.use_exporters(vec![Arc::new(Ledger)]);
    repo.use_value_overrides(Arc::new(ProjectValueOverrides::new()));
    repo.use_activity_sources(Vec::new());
    repo
}

fn project(root: &TempDir) -> ProjectRepo {
    project_with(root, &StateApplyHooks::new())
}

fn snapshot_at(repo: &ProjectRepo, now: DateTime<Utc>) -> String {
    match block_on(repo.snapshot_now(SnapshotReason::Manual, now)).unwrap() {
        SnapshotOutcome::Taken { commit } | SnapshotOutcome::Unchanged { commit } => commit,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

fn put_ledger(repo: &ProjectRepo, key: &str, value: i64) {
    writer(repo)
        .transact(|tx| tx.kv(LEDGER).put(key, toml::Value::Integer(value)))
        .unwrap();
}

fn ledger(repo: &ProjectRepo) -> BTreeMap<String, toml::Value> {
    writer(repo)
        .read(|read| read.kv(LEDGER).scan(""))
        .unwrap()
        .into_iter()
        .collect()
}

fn set_task(repo: &ProjectRepo, id: &str, record: TaskStateRecord) {
    writer(repo).transact(|tx| tx.set_task(id, record)).unwrap();
}

fn task(repo: &ProjectRepo, id: &str) -> Option<TaskStateRecord> {
    writer(repo).read(|read| read.task(id)).unwrap()
}

fn put_run(repo: &ProjectRepo, id: &str, task: &str, state: &str) {
    let mut run = toml::Table::new();
    run.insert("task".into(), toml::Value::String(task.into()));
    run.insert("workflow".into(), toml::Value::String("build".into()));
    run.insert("state".into(), toml::Value::String(state.into()));
    run.insert("outcome".into(), toml::Value::String(String::new()));
    writer(repo)
        .transact(|tx| tx.kv("runs").put(id, toml::Value::Table(run)))
        .unwrap();
}

/// The whole state as text: every task, the runs and the ledger, nothing withheld.
fn everything(repo: &ProjectRepo) -> BTreeMap<String, String> {
    let plain = Secrets::new(&[], &[]).unwrap();
    let exporters: Vec<Arc<dyn StateExporter>> = vec![Arc::new(Ledger)];
    writer(repo)
        .read(|read| snapshot::export(read, &exporters, &plain))
        .unwrap()
        .files
}

fn restore(repo: &ProjectRepo, source: &RestoreSource, now: DateTime<Utc>) -> Result<String> {
    match block_on(repo.restore(source, now))? {
        RestoreOutcome::Restored { replaced, .. } => Ok(replaced),
        RestoreOutcome::Refused { activity } => bail!("refused: {activity:?}"),
    }
}

fn replaced_files(repo: &ProjectRepo) -> Vec<String> {
    repo.replaced_states()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Commits `files` as a snapshot on top of the state branch, as a hand edit would.
fn hand_made_snapshot(repo: &ProjectRepo, files: &[(&str, &str)], now: DateTime<Utc>) -> String {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
    let git = repo.git();
    let repository = &repo.location().repository;
    let previous = block_on(state_branch::tip(git, repository, STATE_REF));
    let committed = block_on(state_branch::commit_snapshot(
        git,
        repository,
        &repo.location().state,
        STATE_REF,
        &files,
        previous.as_deref(),
        &state_branch::snapshot_message("hand", now, 99),
    ))
    .unwrap();
    match committed {
        state_branch::Committed::Moved { commit } => commit,
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// A snapshot whose files are those of the snapshot `base`, with `changes` written over them.
fn edited_snapshot(repo: &ProjectRepo, base: &str, changes: &[(&str, Option<&str>)]) -> String {
    let mut files = crate::snapshot_tests::files_at(repo, base);
    for (path, text) in changes {
        match text {
            Some(text) => {
                files.insert(path.to_string(), text.to_string());
            }
            None => {
                files.remove(*path);
            }
        }
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    hand_made_snapshot(repo, &files, start() + Duration::minutes(30))
}

fn assert_untouched(repo: &ProjectRepo, before: &BTreeMap<String, String>, replaced: usize) {
    assert_eq!(&everything(repo), before);
    assert_eq!(replaced_files(repo).len(), replaced);
    // The store is whole: a new reader reads it.
    let reader = crate::StateStore::open_reader(repo.location()).unwrap();
    reader.read(|read| read.task_ids()).unwrap();
}

/// The fixture project with a snapshot of a known state taken at `start()`, and the live state
/// changed after it.
fn snapshotted(root: &TempDir) -> (ProjectRepo, String) {
    let repo = project(root);
    change_state(&repo, TodoState::Done);
    put_ledger(&repo, "invoices", 3);
    put_run(&repo, "run-1", "l1", "ended");
    let commit = snapshot_at(&repo, start());
    change_state(&repo, TodoState::Open);
    set_task(&repo, "l2", task_with_todos(&[("r1", TodoState::Done)]));
    put_ledger(&repo, "invoices", 4);
    put_ledger(&repo, "credits", 1);
    (repo, commit)
}

#[test]
fn restore_shows_what_it_replaces_then_rebuilds_the_snapshot_and_keeps_the_replaced_state() {
    let root = TempDir::new().unwrap();
    let (repo, commit) = snapshotted(&root);
    let changed = everything(&repo);

    let preview = block_on(repo.restore_preview(&RestoreSource::Newest)).unwrap();
    assert_eq!(preview.source, RestoreSource::Snapshot { commit });
    assert_eq!(preview.kind, SourceKind::Local);
    assert_eq!(preview.taken_at, Some(start()));
    assert_eq!(preview.tasks_differing, ["l1", "l2"]);
    assert_eq!(preview.runs, 1);
    assert_eq!(preview.keep_days, 30);
    let ledger_change = &preview.namespaces[0];
    assert_eq!(
        (ledger_change.namespace.as_str(), ledger_change.differs),
        (LEDGER, true)
    );
    let text = preview.render();
    assert!(text.contains("1 tasks, 1 runs, ledger"), "{text}");
    assert!(text.contains("kept for 30 days"), "{text}");
    // A preview changes nothing.
    assert_eq!(everything(&repo), changed);

    let now = start() + Duration::hours(1);
    let replaced = restore(&repo, &preview.source, now).unwrap();
    assert_eq!(replaced, "20260301T110000Z.db");
    let l1 = task(&repo, "l1").unwrap();
    assert_eq!(l1.todos, vec![todo("t1", TodoState::Done)]);
    assert_eq!(task(&repo, "l2"), None);
    assert_eq!(
        ledger(&repo),
        BTreeMap::from([("invoices".to_string(), toml::Value::Integer(3))])
    );
    let path = repo.location().state.join(REPLACED_DIR).join(&replaced);
    assert!(path.is_file());

    // The replaced state is itself restorable, and restoring it keeps the state it replaces.
    let back = restore(
        &repo,
        &RestoreSource::Replaced { name: replaced },
        now + Duration::minutes(5),
    )
    .unwrap();
    assert_eq!(back, "20260301T110500Z.db");
    assert_eq!(everything(&repo), changed);
    assert_eq!(replaced_files(&repo).len(), 2);
}

#[test]
fn restoring_twice_leaves_the_same_state() {
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    restore(&repo, &RestoreSource::Newest, start() + Duration::hours(1)).unwrap();
    let once = everything(&repo);
    restore(&repo, &RestoreSource::Newest, start() + Duration::hours(1)).unwrap();
    assert_eq!(everything(&repo), once);
    // Two restores in one second keep two replaced states.
    let mut replaced = replaced_files(&repo);
    replaced.sort();
    assert_eq!(replaced, ["20260301T110000Z-2.db", "20260301T110000Z.db"]);
}

#[test]
fn a_restore_runs_no_state_apply_hook_and_keeps_hook_owned_rows_as_imported() {
    let root = TempDir::new().unwrap();
    let hook = Arc::new(CountingLedgerHook {
        calls: AtomicUsize::new(0),
    });
    let hooks = StateApplyHooks::new();
    hooks.register(hook.clone());
    let repo = project_with(&root, &hooks);
    change_state(&repo, TodoState::Done);
    let at_snapshot = ledger(&repo);
    assert_eq!(at_snapshot["hook-count"], toml::Value::Integer(1));
    snapshot_at(&repo, start());
    change_state(&repo, TodoState::Open);
    set_task(&repo, "l2", task_with_todos(&[]));
    assert_eq!(ledger(&repo)["hook-count"], toml::Value::Integer(3));

    hook.calls.store(0, Ordering::SeqCst);
    restore(&repo, &RestoreSource::Newest, start() + Duration::hours(1)).unwrap();
    assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
    assert_eq!(ledger(&repo), at_snapshot);
}

#[test]
fn restore_refuses_while_work_runs_and_can_stop_it_at_a_job_boundary() {
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    let lanes = Arc::new(FakeActivity {
        running: Mutex::new(vec!["lane 1 runs build of l1".to_string()]),
    });
    repo.use_activity_sources(vec![lanes]);
    let before = everything(&repo);

    let preview = block_on(repo.restore_preview(&RestoreSource::Newest)).unwrap();
    assert_eq!(preview.activity, ["lanes: lane 1 runs build of l1"]);
    assert!(preview.render().contains("Work is running in this project"));
    let outcome = block_on(repo.restore(&RestoreSource::Newest, start())).unwrap();
    assert_eq!(
        outcome,
        RestoreOutcome::Refused {
            activity: vec!["lanes: lane 1 runs build of l1".to_string()]
        }
    );
    assert_untouched(&repo, &before, 0);

    assert!(repo.stop_at_job_boundary().is_empty());
    restore(&repo, &RestoreSource::Newest, start()).unwrap();
    assert_eq!(task(&repo, "l2"), None);
}

#[test]
fn a_source_that_cannot_be_stopped_says_so() {
    struct Stubborn;
    impl ActivitySource for Stubborn {
        fn name(&self) -> &str {
            "runs"
        }
        fn activity(&self, _project_id: &str) -> Vec<String> {
            vec!["run 7".to_string()]
        }
    }
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    repo.use_activity_sources(vec![Arc::new(Stubborn)]);
    let problems = repo.stop_at_job_boundary();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("runs cannot be stopped from here"));
}

#[test]
fn missing_worktrees_and_run_records_are_marked_and_nothing_is_recreated() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let gone = root.path().join("worktrees").join("backfill");
    let here = root.path().join("worktrees").join("report");
    std::fs::create_dir_all(&here).unwrap();
    let mut l1 = task_with_todos(&[("t1", TodoState::Open)]);
    l1.worktree = Some(gone.to_string_lossy().into_owned());
    set_task(&repo, "l1", l1);
    let mut l2 = task_with_todos(&[("r1", TodoState::Open)]);
    l2.worktree = Some(here.to_string_lossy().into_owned());
    set_task(&repo, "l2", l2);
    let mut done = task_with_todos(&[]);
    done.status = Status::Done;
    done.worktree = Some("relative/never/looked/up".to_string());
    set_task(&repo, "g1", done);
    put_run(&repo, "run-1", "l1", "running");
    put_run(&repo, "run-2", "l2", "ended");
    put_run(&repo, "run-3", "../../escape", "running");
    let records = repo.location().files.join("l2").join("runs");
    std::fs::create_dir_all(&records).unwrap();
    std::fs::write(records.join("run-2.jsonl"), "{}\n").unwrap();
    snapshot_at(&repo, start());

    let preview = block_on(repo.restore_preview(&RestoreSource::Newest)).unwrap();
    let worktrees: Vec<&str> = preview
        .missing_worktrees
        .iter()
        .map(|missing| missing.task.as_str())
        .collect();
    assert_eq!(worktrees, ["l1"]);
    let runs: Vec<(&str, bool)> = preview
        .missing_run_records
        .iter()
        .map(|missing| (missing.run.as_str(), missing.active))
        .collect();
    assert_eq!(runs, [("run-1", true), ("run-3", true)]);
    assert!(
        preview
            .render()
            .contains("1 worktrees and 2 run records are not on this machine")
    );

    restore(&repo, &RestoreSource::Newest, start()).unwrap();
    let marks = repo.missing_marks().unwrap();
    assert_eq!(
        marks.get("l1").unwrap(),
        &vec![WORKTREE_MISSING.to_string(), RUN_RECORD_MISSING.to_string()]
    );
    assert_eq!(marks.get("l2"), None);
    assert_eq!(marks.get("g1"), None);
    assert!(!gone.exists());
    assert!(!repo.location().files.join("l1").exists());
}

#[test]
fn withheld_values_are_never_imported() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let mut l1 = task_with_todos(&[("t1", TodoState::Done)]);
    l1.branch = Some("work/SECRET-123456".to_string());
    set_task(&repo, "l1", l1);
    let mut l2 = task_with_todos(&[]);
    l2.base = Some("tok_abcdefgh".to_string());
    set_task(&repo, "l2", l2);
    put_ledger(&repo, "plain", 1);
    writer(&repo)
        .transact(|tx| {
            tx.kv(LEDGER)
                .put("note", toml::Value::String("SECRET-111111".to_string()))
        })
        .unwrap();
    let commit = snapshot_at(&repo, start());
    let files = crate::snapshot_tests::files_at(&repo, &commit);
    assert!(files["tasks/l1.toml"].contains(WITHHELD_SECRET));
    // This machine now has another value for l1, and nothing for l2 or the note.
    let mut l1 = task_with_todos(&[("t1", TodoState::Done)]);
    l1.branch = Some("work/SECRET-654321".to_string());
    set_task(&repo, "l1", l1);
    writer(&repo)
        .transact(|tx| {
            tx.delete_task("l2")?;
            tx.kv(LEDGER).delete("note")
        })
        .unwrap();

    let preview = block_on(repo.restore_preview(&RestoreSource::Newest)).unwrap();
    let notes: Vec<(&str, &str, bool)> = preview
        .withheld
        .iter()
        .map(|note| (note.file.as_str(), note.location.as_str(), note.kept_local))
        .collect();
    assert!(notes.contains(&(LEDGER_FILE, "note", false)), "{notes:?}");
    assert!(
        notes.contains(&("tasks/l1.toml", "branch", true)),
        "{notes:?}"
    );
    assert!(
        notes.contains(&("tasks/l2.toml", "base", false)),
        "{notes:?}"
    );
    assert!(
        preview
            .render()
            .contains("the value of this machine is kept")
    );

    restore(&repo, &RestoreSource::Newest, start()).unwrap();
    assert_eq!(
        task(&repo, "l1").unwrap().branch.as_deref(),
        Some("work/SECRET-654321")
    );
    assert_eq!(task(&repo, "l2").unwrap().base, None);
    assert_eq!(ledger(&repo).get("note"), None);
    assert_eq!(ledger(&repo)["plain"], toml::Value::Integer(1));
    for text in everything(&repo).values() {
        assert!(!text.contains(WITHHELD_SECRET), "{text}");
    }
}

#[test]
fn a_cut_short_or_edited_snapshot_changes_nothing() {
    let root = TempDir::new().unwrap();
    let (repo, base) = snapshotted(&root);
    let before = everything(&repo);
    let cases: Vec<(&str, Vec<(&str, Option<&str>)>, &str)> = vec![
        (
            "cut short",
            vec![("tasks/l1.toml", Some("status = \"op"))],
            "tasks/l1.toml is not valid TOML",
        ),
        (
            "edited",
            vec![("tasks/l1.toml", Some("status = \"flying\"\n"))],
            "tasks/l1.toml is not a task state",
        ),
        (
            "no format",
            vec![("FORMAT", None)],
            "this is not a state snapshot",
        ),
        (
            "another kind",
            vec![("FORMAT", Some("other-state 1\n"))],
            "this is not a state snapshot",
        ),
        (
            "runs cut short",
            vec![("runs.toml", Some("[[run]]\ntask = \"l1\"\n"))],
            "a run without an id",
        ),
        (
            "withheld list cut short",
            vec![("withheld.toml", Some("[[withheld]]\nfile = "))],
            "withheld.toml is not valid",
        ),
        (
            "a stray task file",
            vec![("tasks/nested/l1.toml", Some("status = \"open\"\n"))],
            "which is not a task file",
        ),
        (
            "an importer refuses",
            vec![(LEDGER_FILE, Some("refuse = true\n"))],
            "the ledger refuses this file",
        ),
    ];
    for (name, changes, expected) in cases {
        let commit = edited_snapshot(&repo, &base, &changes);
        let error = restore(&repo, &RestoreSource::Snapshot { commit }, start())
            .expect_err(name)
            .to_string();
        let error = format!("{error:#}");
        assert!(error.contains(expected), "{name}: {error}");
        assert_untouched(&repo, &before, 0);
    }
}

#[test]
fn a_newer_format_is_refused_with_a_clear_message() {
    let root = TempDir::new().unwrap();
    let (repo, base) = snapshotted(&root);
    let before = everything(&repo);
    let commit = edited_snapshot(&repo, &base, &[("FORMAT", Some("bongos-state 2\n"))]);
    let error = restore(&repo, &RestoreSource::Snapshot { commit }, start()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the snapshot is format version 2, newer than this bongos reads (version 1); \
         update bongos to restore it"
    );
    assert_untouched(&repo, &before, 0);
    assert_eq!(
        format!("{} {}", crate::FORMAT_KIND, crate::FORMAT_VERSION),
        crate::FORMAT_LINE
    );
}

#[test]
fn hostile_paths_in_a_snapshot_are_refused_whole() {
    for (path, problem) in [
        ("../state.db", "climbs out with `..`"),
        ("/etc/passwd", "is absolute"),
        ("ledger/../../x", "climbs out with `..`"),
        (".git/config", "names a git file"),
        ("ledger/.GIT", "names a git file"),
        ("ledger//x", "has an empty or invalid component"),
        ("ledger/a\\b", "has an empty or invalid component"),
        ("ledger/x\ny", "has an empty or invalid component"),
    ] {
        assert_eq!(snapshot_path_problem(path), Some(problem), "{path:?}");
    }
    assert_eq!(snapshot_path_problem("ledger/entries.toml"), None);
    assert_eq!(snapshot_path_problem("tasks/l1.toml"), None);

    // Through git: a symbolic link, and a folder named `..` made with git's own tools.
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    let before = everything(&repo);
    let git = repo.git();
    let repository = &repo.location().repository;
    let index = root.path().join("mktree.index");
    let tree_of = |entries: &str| -> String {
        // Git's own tool, which a person editing the branch by hand could use.
        match block_on(git.run_on_index(
            repository,
            &index,
            &["mktree"],
            Some(entries.as_bytes().to_vec()),
        )) {
            Ok(output) => String::from_utf8(output).unwrap().trim().to_string(),
            Err(_) => String::new(),
        }
    };
    let blob = |text: &str| -> String {
        let path = root.path().join("blob");
        std::fs::write(&path, text).unwrap();
        block_on(git.run(repository, &["hash-object", "-w", path.to_str().unwrap()])).unwrap()
    };
    let format = blob("bongos-state 1\n");
    let target = blob("/etc/passwd");
    let inner = tree_of(&format!("100644 blob {format}\tpasswd\n"));
    for (name, entries) in [
        (
            "a link",
            format!("100644 blob {format}\tFORMAT\n120000 blob {target}\tledger\n"),
        ),
        (
            "a climbing folder",
            format!("100644 blob {format}\tFORMAT\n040000 tree {inner}\t..\n"),
        ),
    ] {
        let tree = tree_of(&entries);
        if tree.is_empty() {
            // This git refuses to make such a tree at all.
            continue;
        }
        let commit = block_on(git.commit_tree(
            repository,
            &tree,
            &[],
            &state_branch::snapshot_message("hostile", start(), 1),
            &crate::Identity::default(),
        ))
        .unwrap();
        let tip = block_on(git.rev_parse(repository, STATE_REF)).unwrap();
        block_on(git.update_ref_cas(repository, STATE_REF, &commit, &tip)).unwrap();
        let error = restore(&repo, &RestoreSource::Snapshot { commit }, start()).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("nothing was restored"), "{name}: {error}");
        assert_untouched(&repo, &before, 0);
    }
}

#[test]
fn format_and_replaced_names_are_checked() {
    let files = |format: &str| BTreeMap::from([("FORMAT".to_string(), format.to_string())]);
    assert!(check_format(&files("bongos-state 1\n")).is_ok());
    assert!(check_format(&files("bongos-state 0\n")).is_err());
    assert!(check_format(&files("bongos-state one\n")).is_err());
    assert!(check_format(&files("bongos-state\n")).is_err());
    assert!(check_format(&BTreeMap::new()).is_err());

    assert_eq!(
        replaced_time("20260301T100000Z.db"),
        Some(Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap())
    );
    assert!(replaced_time("20260301T100000Z-2.db").is_some());
    for name in [
        "../20260301T100000Z.db",
        "20260301T100000Z.db/x",
        "20260301T100000Z-.db",
        "20260301T100000Z-x.db",
        "state.db",
        "20261301T100000Z.db",
        "20260301T100000Z.sqlite",
    ] {
        assert_eq!(replaced_time(name), None, "{name}");
    }
}

#[test]
fn restore_never_interleaves_with_a_landing() {
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    let before = everything(&repo);
    // A landing holds the landing lock.
    repo.take_lock_for("change-1").unwrap();
    let error = restore(&repo, &RestoreSource::Newest, start()).unwrap_err();
    assert!(
        error.to_string().contains("a landing is in progress"),
        "{error:#}"
    );
    assert_untouched(&repo, &before, 0);
    repo.release_lock("change-1");
    // Once the landing let go, the restore goes through and the landing waits for it.
    restore(&repo, &RestoreSource::Newest, start()).unwrap();
    assert_eq!(task(&repo, "l2"), None);
    assert!(repo.take_lock_for("change-2").is_ok());
    repo.release_lock("change-2");
}

/// A run starter that refuses to start work while a restore holds it, and logs what it saw.
#[derive(Default)]
struct GatedRuns {
    holds: AtomicUsize,
    log: Mutex<Vec<String>>,
    /// Writes to the state when the hold is taken, as a job that just ended would.
    on_hold: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl GatedRuns {
    fn held(&self) -> bool {
        self.holds.load(Ordering::SeqCst) > 0
    }

    /// Starts a run unless new work is held.
    fn try_start(&self, run: &str) -> bool {
        let started = !self.held();
        self.log.lock().push(format!("start {run}: {started}"));
        started
    }
}

impl ActivitySource for GatedRuns {
    fn name(&self) -> &str {
        "runs"
    }

    fn activity(&self, _project_id: &str) -> Vec<String> {
        self.log
            .lock()
            .push(format!("activity checked, held {}", self.held()));
        Vec::new()
    }

    fn hold_new_work(&self, _project_id: &str) {
        self.holds.fetch_add(1, Ordering::SeqCst);
        self.log.lock().push("hold".to_string());
        if let Some(write) = self.on_hold.lock().as_ref() {
            write();
        }
    }

    fn release_new_work(&self, _project_id: &str) {
        self.holds.fetch_sub(1, Ordering::SeqCst);
        self.log.lock().push("release".to_string());
    }
}

/// The ledger's exporter, whose import tries to start a run first, then imports, fails or
/// panics.
struct StartsARun {
    runs: Arc<GatedRuns>,
    ending: &'static str,
}

impl StateExporter for StartsARun {
    fn namespace(&self) -> &str {
        LEDGER
    }

    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        Ledger.export(read)
    }

    fn import(&self, tx: &mut StateTx, files: &[(RelPathBuf, String)]) -> Result<()> {
        self.runs.try_start("run 9");
        match self.ending {
            "fail" => bail!("the ledger cannot import"),
            "panic" => panic!("the ledger importer panicked"),
            _ => Ledger.import(tx, files),
        }
    }
}

#[test]
fn new_work_is_held_from_the_activity_check_until_the_import_ended() {
    for ending in ["import", "fail", "panic"] {
        let root = TempDir::new().unwrap();
        let (repo, _) = snapshotted(&root);
        let runs = Arc::new(GatedRuns::default());
        repo.use_activity_sources(vec![runs.clone()]);
        repo.use_exporters(vec![Arc::new(StartsARun {
            runs: runs.clone(),
            ending,
        })]);

        let restored = restore(&repo, &RestoreSource::Newest, start() + Duration::hours(1));
        assert_eq!(
            restored.is_ok(),
            ending == "import",
            "{ending}: {restored:?}"
        );
        assert_eq!(
            *runs.log.lock(),
            [
                "activity checked, held false",
                "hold",
                "activity checked, held true",
                "start run 9: false",
                "release",
            ],
            "{ending}"
        );
        // Held work starts once the restore ended, however it ended, and a landing is not
        // blocked by the restore.
        assert!(runs.try_start("run 10"), "{ending}");
        assert!(repo.take_lock_for("change-1").is_ok(), "{ending}");
        repo.release_lock("change-1");
    }
}

#[test]
fn the_kept_tasks_and_withheld_stand_ins_are_read_when_the_import_runs() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let mut l1 = task_with_todos(&[("t1", TodoState::Done)]);
    l1.branch = Some("work/SECRET-123456".to_string());
    set_task(&repo, "l1", l1);
    snapshot_at(&repo, start());
    let mut l1 = task_with_todos(&[("t1", TodoState::Done)]);
    l1.branch = Some("work/SECRET-222222".to_string());
    set_task(&repo, "l1", l1);
    let preview = block_on(repo.restore_preview(&RestoreSource::Newest)).unwrap();
    assert!(preview.withheld.iter().any(|note| note.kept_local));

    // After the preview, and after the restore read the snapshot, a task no snapshot can hold
    // and a plain task are created, and the local value of the withheld branch changes.
    let runs = Arc::new(GatedRuns::default());
    let store = writer(&repo);
    *runs.on_hold.lock() = Some(Box::new(move || {
        store
            .transact(|tx| {
                tx.set_task("SECRET-999999", task_with_todos(&[]))?;
                tx.set_task("l9", task_with_todos(&[]))?;
                let mut l1 = tx.task("l1")?.unwrap();
                l1.branch = Some("work/SECRET-333333".to_string());
                tx.set_task("l1", l1)
            })
            .unwrap();
    }));
    repo.use_activity_sources(vec![runs]);

    let outcome = block_on(repo.restore(&RestoreSource::Newest, start())).unwrap();
    let RestoreOutcome::Restored { preview, .. } = outcome else {
        panic!("expected a restore, got {outcome:?}");
    };
    assert_eq!(
        task(&repo, "l1").unwrap().branch.as_deref(),
        Some("work/SECRET-333333")
    );
    assert!(task(&repo, "SECRET-999999").is_some());
    assert_eq!(task(&repo, "l9"), None);
    assert!(
        preview
            .notes
            .iter()
            .any(|note| note.contains("1 tasks of this machine have ids no snapshot can hold")),
        "{:?}",
        preview.notes
    );
}

#[test]
fn a_remote_state_branch_rewritten_by_thinning_still_restores() {
    let root = TempDir::new().unwrap();
    let (repo, remote) = crate::add_existing_tests::pushed(&root);
    repo.use_activity_sources(Vec::new());
    let local = block_on(state_branch::tip(
        repo.git(),
        &repo.location().repository,
        STATE_REF,
    ))
    .unwrap();
    // The remote's state branch has been fetched once.
    let candidates = block_on(repo.restore_candidates()).unwrap();
    assert!(candidates.notes.is_empty(), "{:?}", candidates.notes);

    // Thinning rewrote it on the remote: a newer snapshot without the old one as parent.
    let files = crate::snapshot_tests::files_at(&repo, &local);
    let thinned = match block_on(state_branch::commit_snapshot(
        repo.git(),
        &repo.location().repository,
        &repo.location().state,
        "refs/heads/thinned",
        &files,
        None,
        &state_branch::snapshot_message("thinning", start() + Duration::hours(2), 1),
    ))
    .unwrap()
    {
        state_branch::Committed::Moved { commit } => commit,
        other => panic!("expected a commit, got {other:?}"),
    };
    crate::test_support::run_git(
        &repo,
        &[
            "push",
            "-q",
            "-f",
            remote.to_str().unwrap(),
            &format!("{thinned}:{STATE_REF}"),
        ],
    );
    assert!(!block_on(repo.git().is_ancestor(
        &repo.location().repository,
        &local,
        &thinned
    )));
    change_state(&repo, TodoState::Open);

    let candidates = block_on(repo.restore_candidates()).unwrap();
    assert!(candidates.notes.is_empty(), "{:?}", candidates.notes);
    assert_eq!(
        candidates.newest,
        Some(RestoreSource::Snapshot {
            commit: thinned.clone()
        })
    );
    let outcome =
        block_on(repo.restore(&RestoreSource::Newest, start() + Duration::hours(3))).unwrap();
    let RestoreOutcome::Restored { preview, .. } = outcome else {
        panic!("expected a restore, got {outcome:?}");
    };
    assert_eq!(preview.kind, SourceKind::Remote);
    assert_eq!(preview.source, RestoreSource::Snapshot { commit: thinned });
    assert_eq!(
        task(&repo, "l1").unwrap().todos,
        vec![todo("t1", TodoState::Done)]
    );
}

#[test]
fn old_replaced_states_are_removed_after_the_keep_days() {
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    let mut options = repo.backup_options();
    options.replaced_keep_days = 7;
    repo.set_backup_options(options);
    let dir = repo.location().state.join(REPLACED_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["20260215T100000Z.db", "20260225T100000Z.db", "notes.txt"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    restore(&repo, &RestoreSource::Newest, start()).unwrap();
    assert_eq!(
        replaced_files(&repo),
        ["20260301T100000Z.db", "20260225T100000Z.db"]
    );
    assert!(dir.join("notes.txt").exists());
}

#[test]
fn an_unknown_source_is_refused() {
    let root = TempDir::new().unwrap();
    let (repo, _) = snapshotted(&root);
    let before = everything(&repo);
    let main = crate::test_support::main_commit(&repo);
    for (source, expected) in [
        (
            RestoreSource::Snapshot {
                commit: "main".to_string(),
            },
            "is not a snapshot's commit id",
        ),
        (
            RestoreSource::Snapshot { commit: main },
            "is not a snapshot of this project",
        ),
        (
            RestoreSource::Replaced {
                name: "../../state.db".to_string(),
            },
            "is not the name of a replaced state",
        ),
        (
            RestoreSource::Replaced {
                name: "20260301T100000Z.db".to_string(),
            },
            "there is no replaced state",
        ),
    ] {
        let error = restore(&repo, &source, start()).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "{source:?}: {error:#}"
        );
        assert_untouched(&repo, &before, 0);
    }
    // A project without any snapshot has nothing to restore.
    let empty_root = TempDir::new().unwrap();
    let empty = project(&empty_root);
    let error = restore(&empty, &RestoreSource::Newest, start()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("there is no snapshot to restore")
    );
}

fn window() -> PeerIdentity {
    PeerIdentity::for_test(4242, ClientKind::Window, true)
}

fn cli() -> PeerIdentity {
    PeerIdentity::for_test(5151, ClientKind::Cli, false)
}

fn send(repo: &ProjectRepo, command: serde_json::Value, peer: &PeerIdentity) -> Result<RepoReply> {
    let command: RepoCommand = serde_json::from_value(command)?;
    block_on(HostBoundary::new(repo.clone()).accept_now(command, peer, start()))
}

#[test]
fn restore_commands_cross_the_host_boundary_and_only_a_window_restores() {
    let root = TempDir::new().unwrap();
    let (repo, commit) = snapshotted(&root);
    let before = everything(&repo);
    let person = serde_json::json!({"name": "Kyle"});
    for command in [
        RepoCommand::RestoreCandidates,
        RepoCommand::RestorePreview {
            source: RestoreSource::Newest,
        },
        RepoCommand::Restore {
            source: RestoreSource::Replaced {
                name: "20260301T100000Z.db".to_string(),
            },
            person: crate::PersonClaim {
                name: "Kyle".to_string(),
            },
        },
        RepoCommand::StopAtJobBoundary {
            person: crate::PersonClaim {
                name: "Kyle".to_string(),
            },
        },
    ] {
        let wired: RepoCommand =
            serde_json::from_value(serde_json::to_value(&command).unwrap()).unwrap();
        assert_eq!(wired, command);
    }

    let RepoReply::RestoreCandidates { candidates } = send(
        &repo,
        serde_json::json!({"command": "restore-candidates"}),
        &cli(),
    )
    .unwrap() else {
        panic!("expected candidates");
    };
    assert_eq!(
        candidates.newest,
        Some(RestoreSource::Snapshot {
            commit: commit.clone()
        })
    );
    let reply = send(
        &repo,
        serde_json::json!({"command": "restore-preview", "source": {"source": "newest"}}),
        &cli(),
    )
    .unwrap();
    let wired: RepoReply = serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
    assert_eq!(wired, reply);

    // Only a verified window restores; anything else changes nothing.
    let restore_command = serde_json::json!({
        "command": "restore",
        "source": {"source": "snapshot", "commit": commit},
        "person": person,
    });
    let error = send(&repo, restore_command.clone(), &cli()).unwrap_err();
    assert_eq!(error.to_string(), RESTORE_NEEDS_A_WINDOW);
    assert_untouched(&repo, &before, 0);
    let error = send(
        &repo,
        serde_json::json!({"command": "stop-at-job-boundary", "person": person}),
        &cli(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), RESTORE_NEEDS_A_WINDOW);

    // Hostile and malformed sources are refused before anything changes.
    for command in [
        serde_json::json!({"command": "restore", "person": person,
            "source": {"source": "replaced", "name": "../../../etc/passwd"}}),
        serde_json::json!({"command": "restore", "person": person,
            "source": {"source": "snapshot", "commit": "--upload-pack=touch /tmp/x"}}),
        serde_json::json!({"command": "restore", "person": person,
            "source": {"source": "anything"}}),
        serde_json::json!({"command": "restore", "source": {"source": "newest"}}),
    ] {
        assert!(
            send(&repo, command.clone(), &window()).is_err(),
            "{command}"
        );
        assert_untouched(&repo, &before, 0);
    }

    let RepoReply::Restore { outcome } = send(&repo, restore_command, &window()).unwrap() else {
        panic!("expected a restore");
    };
    assert!(
        matches!(outcome, RestoreOutcome::Restored { .. }),
        "{outcome:?}"
    );
    assert_eq!(task(&repo, "l2"), None);
}

/// Every non-comment line of `sources` (file name, text) that names `import_without_hooks`
/// outside the files allowed to call it, but for its definition.
pub(crate) fn unallowed_import_calls(sources: &[(String, String)]) -> Vec<String> {
    const ALLOWED: [&str; 2] = ["restore.rs", "convert.rs"];
    let name = concat!("import_without", "_hooks");
    let definition = format!("fn {name}");
    let mut found = Vec::new();
    for (file, text) in sources {
        if ALLOWED.contains(&file.as_str()) {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || line.contains(&definition) {
                continue;
            }
            if line.contains(name) {
                found.push(format!("{file}:{}", number + 1));
            }
        }
    }
    found
}

#[test]
fn only_restore_and_conversion_write_without_the_hooks() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&path).unwrap(),
            ));
        }
    }
    assert!(sources.iter().any(|(file, _)| file == "restore.rs"));
    assert!(sources.iter().any(|(file, _)| file == "convert.rs"));
    assert_eq!(unallowed_import_calls(&sources), Vec::<String>::new());
}

#[test]
fn the_source_scan_finds_a_call_outside_restore_and_conversion() {
    let call = format!(
        "    writer.{}(ImportReason::Restore, None, |tx| Ok(()))?;\n",
        concat!("import_without", "_hooks")
    );
    let sources = vec![
        (
            "landing.rs".to_string(),
            format!("fn land() {{\n{call}}}\n"),
        ),
        ("restore.rs".to_string(), call.clone()),
        ("convert.rs".to_string(), call.clone()),
        (
            "state.rs".to_string(),
            format!(
                "    pub(crate) fn {}<T>(\n",
                concat!("import_without", "_hooks")
            ),
        ),
        ("notes.rs".to_string(), format!("// {call}")),
    ];
    assert_eq!(unallowed_import_calls(&sources), ["landing.rs:2"]);
}
