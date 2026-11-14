use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::{HistoryEntry, Status, TodoState};
use chrono::{DateTime, TimeZone as _, Utc};
use futures::executor::block_on;
use tempfile::TempDir;
use util::rel_path::{RelPath, RelPathBuf};

use crate::backup::{BackupOptions, SnapshotOutcome, SnapshotReason};
use crate::snapshot::{
    self, FORMAT_LINE, RUNS_NAMESPACE, Secrets, WITHHELD_SECRET, exported_path_problem,
    namespace_problem,
};
use crate::state_branch::{self, Committed, SNAPSHOT_INDEX_FILE, state_ref};
use crate::test_support::{fixture_project, task_with_todos, todo};
use crate::{ProjectRepo, StateExporter, StateRead, StateStore, StateTx, StateWriter};

/// The secret patterns of these tests: shapes no secret scanner knows, so the fixtures never
/// look like real keys.
pub(crate) const PATTERNS: [&str; 4] = [
    "SECRET-[0-9]{6}",
    "tok_[a-z]{8}",
    "[0-9]{6}-TAIL",
    "ключ-[а-я]{4}",
];

pub(crate) fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap()
}

pub(crate) fn test_options() -> BackupOptions {
    BackupOptions {
        secret_patterns: PATTERNS.map(str::to_string).to_vec(),
        secret_keys: vec!["linear.token".to_string(), "password".to_string()],
        ..BackupOptions::default()
    }
}

/// The fixture project with the test backup options and no exporters.
pub(crate) fn project(root: &TempDir) -> ProjectRepo {
    let repo = fixture_project(root);
    repo.set_backup_options(test_options());
    repo.use_exporters(Vec::new());
    repo.use_value_overrides(Arc::new(crate::ProjectValueOverrides::new()));
    repo
}

pub(crate) fn writer(repo: &ProjectRepo) -> Arc<StateWriter> {
    repo.writer_or_error().unwrap()
}

/// Writes a todo state of task `l1`: a change of the state.
pub(crate) fn change_state(repo: &ProjectRepo, state: TodoState) {
    writer(repo)
        .transact(|tx| tx.set_todo("l1", todo("t1", state)))
        .unwrap();
}

/// The files of the tree of `treeish`, by path.
pub(crate) fn files_at(repo: &ProjectRepo, treeish: &str) -> BTreeMap<String, String> {
    let git = repo.git();
    let repository = &repo.location().repository;
    block_on(async {
        let entries = git.ls_tree_r(repository, treeish).await.unwrap();
        let objects: Vec<String> = entries.iter().map(|entry| entry.object.clone()).collect();
        let blobs = git.cat_file_batch(repository, &objects).await.unwrap();
        entries
            .into_iter()
            .zip(blobs)
            .map(|(entry, blob)| (entry.path, String::from_utf8(blob.unwrap()).unwrap()))
            .collect()
    })
}

pub(crate) fn state_files(repo: &ProjectRepo) -> BTreeMap<String, String> {
    files_at(repo, "refs/heads/bongos/state")
}

fn exporters(list: Vec<Arc<dyn StateExporter>>) -> Vec<Arc<dyn StateExporter>> {
    list
}

fn path(text: &str) -> RelPathBuf {
    RelPath::unix(text).unwrap().to_rel_path_buf()
}

/// An exporter that gives fixed files under a fixed namespace.
struct Fixed {
    namespace: String,
    files: Vec<(RelPathBuf, String)>,
}

impl StateExporter for Fixed {
    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn export(&self, _read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        Ok(self.files.clone())
    }

    fn import(&self, _tx: &mut StateTx, _files: &[(RelPathBuf, String)]) -> Result<()> {
        Ok(())
    }
}

fn fixed(namespace: &str, files: &[(&str, &str)]) -> Arc<dyn StateExporter> {
    Arc::new(Fixed {
        namespace: namespace.to_string(),
        files: files
            .iter()
            .map(|(name, text)| (path(name), text.to_string()))
            .collect(),
    })
}

struct Failing;

impl StateExporter for Failing {
    fn namespace(&self) -> &str {
        "failing"
    }

    fn export(&self, _read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        Err(anyhow!("the exporter broke on SECRET-424242"))
    }

    fn import(&self, _tx: &mut StateTx, _files: &[(RelPathBuf, String)]) -> Result<()> {
        Ok(())
    }
}

struct Panicking;

impl StateExporter for Panicking {
    fn namespace(&self) -> &str {
        "panicking"
    }

    fn export(&self, _read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        panic!("an exporter bug")
    }

    fn import(&self, _tx: &mut StateTx, _files: &[(RelPathBuf, String)]) -> Result<()> {
        Ok(())
    }
}

fn secrets() -> Secrets {
    Secrets::new(&test_options().secret_patterns, &test_options().secret_keys).unwrap()
}

fn history(detail: &str) -> HistoryEntry {
    toml::from_str(&format!(
        "at = 2026-02-01T09:00:00Z\nby = \"kyle\"\nevent = \"note\"\ndetail = {}\n",
        toml::Value::String(detail.to_string())
    ))
    .unwrap()
}

fn assert_no_secret(files: &BTreeMap<String, String>, raw: &[&str]) {
    let patterns: Vec<regex::Regex> = PATTERNS
        .iter()
        .map(|pattern| regex::Regex::new(pattern).unwrap())
        .collect();
    for (path, text) in files {
        for pattern in &patterns {
            assert!(!pattern.is_match(path), "{path} matches {pattern}");
            assert!(!pattern.is_match(text), "{path} holds a match of {pattern}");
        }
        for secret in raw {
            assert!(
                !path.contains(secret) && !text.contains(secret),
                "{path} holds {secret}"
            );
        }
    }
}

#[test]
fn a_snapshot_is_plain_text_with_the_format_marker_task_states_and_runs() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let mut record = task_with_todos(&[("t1", TodoState::Done), ("t2", TodoState::Open)]);
    record.history.push(history("started the backfill"));
    writer(&repo)
        .transact(|tx| {
            tx.set_task("l1", record.clone())?;
            tx.kv(RUNS_NAMESPACE).put(
                "run-1",
                toml::Value::Table(toml::toml! {
                    task = "l1"
                    workflow = "ship"
                    state = "ended"
                    outcome = "passed"
                }),
            )
        })
        .unwrap();

    let outcome = block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
    assert!(
        matches!(outcome, SnapshotOutcome::Taken { .. }),
        "{outcome:?}"
    );

    let files = state_files(&repo);
    assert_eq!(files["FORMAT"].trim(), FORMAT_LINE);
    let task: TaskStateRecord = toml::from_str(&files["tasks/l1.toml"]).unwrap();
    assert_eq!(task, record);
    let runs: toml::Table = toml::from_str(&files["runs.toml"]).unwrap();
    let run = &runs["run"].as_array().unwrap()[0];
    assert_eq!(run["id"].as_str(), Some("run-1"));
    assert_eq!(run["workflow"].as_str(), Some("ship"));
    assert_eq!(run["outcome"].as_str(), Some("passed"));
    assert!(files.contains_key("withheld.toml"));
    // Only state: no definitions, no run record files.
    assert!(
        files
            .keys()
            .all(|path| !path.starts_with("definitions") && !path.contains("runs/")),
        "{:?}",
        files.keys()
    );
}

#[test]
fn a_snapshot_holds_only_this_projects_state() {
    let root = TempDir::new().unwrap();
    let other_root = TempDir::new().unwrap();
    let repo = project(&root);
    let other = project(&other_root);
    writer(&other)
        .transact(|tx| {
            tx.set_task("other-task", task_with_todos(&[("o1", TodoState::Done)]))?;
            tx.kv(RUNS_NAMESPACE).put(
                "pool-run",
                toml::Value::Table(toml::toml! { task = "other-task" }),
            )
        })
        .unwrap();
    change_state(&repo, TodoState::Done);
    // A run record file of this project stays out too.
    let records = repo.location().files.join("l1").join("runs");
    std::fs::create_dir_all(&records).unwrap();
    std::fs::write(records.join("run-1.jsonl"), "{}\n").unwrap();

    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    let files = state_files(&repo);
    assert!(files.contains_key("tasks/l1.toml"));
    for (path, text) in &files {
        assert!(
            !path.contains("other-task") && !text.contains("other-task"),
            "{path}"
        );
        assert!(!text.contains("pool-run"), "{path}");
        assert!(!path.ends_with(".jsonl"), "{path}");
    }
}

#[test]
fn secrets_are_withheld_everywhere_and_listed_without_their_values() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let mut record = task_with_todos(&[("todo-tok_abcdefgh", TodoState::Done)]);
    record.history.push(history(
        "keys SECRET-111111 and SECRET-222222, then SECRET-333333-TAIL and ключ-абвг 日本",
    ));
    record.branch = Some("feature/tok_qwertyui".to_string());
    writer(&repo)
        .transact(|tx| {
            tx.set_task("l1", record)?;
            tx.set_task("SECRET-999999", TaskStateRecord::default())
        })
        .unwrap();
    repo.use_exporters(exporters(vec![fixed(
        "linear",
        &[
            (
                "linear/settings.toml",
                "token = \"plain\"\n[linear]\ntoken = \"abc\"\n[map]\n\"SECRET-555555\" = 1\nnote = \"tok_zzzzzzzz\"\n",
            ),
            ("linear/SECRET-777777/notes.txt", "fine"),
            ("linear/log.txt", "line one\nSECRET-888888 on line two\n"),
        ],
    )]));

    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    let files = state_files(&repo);
    let raw = [
        "tok_abcdefgh",
        "SECRET-111111",
        "SECRET-222222",
        "SECRET-333333",
        "ключ-абвг",
        "tok_qwertyui",
        "SECRET-999999",
        "SECRET-555555",
        "tok_zzzzzzzz",
        "SECRET-777777",
        "SECRET-888888",
        "\"abc\"",
    ];
    assert_no_secret(&files, &raw);
    // Two matches in one string, an overlapping pair merged, unicode kept around them.
    let task = &files["tasks/l1.toml"];
    assert!(task.contains(&format!(
        "keys {WITHHELD_SECRET} and {WITHHELD_SECRET}, then {WITHHELD_SECRET} and {WITHHELD_SECRET} 日本"
    )));
    assert!(task.contains("feature/<withheld: secret>"));
    // The schema's secret key is withheld whatever its value; a key of another name is not.
    let settings: toml::Table = toml::from_str(&files["linear/settings.toml"]).unwrap();
    assert_eq!(settings["linear"]["token"].as_str(), Some(WITHHELD_SECRET));
    assert_eq!(settings["token"].as_str(), Some("plain"));
    assert!(files.contains_key("linear/withheld-1/notes.txt"));
    assert!(files.keys().any(|path| path.starts_with("tasks/withheld-")));

    let withheld = &files["withheld.toml"];
    for location in ["history", "linear.token", "(key)", "file name", "task id"] {
        assert!(
            withheld.contains(location),
            "{location} missing from {withheld}"
        );
    }
    let status = block_on(repo.backup_status(start())).unwrap();
    assert!(status.withheld.len() >= 8, "{:?}", status.withheld);
    let shown = serde_json::to_string(&status).unwrap() + &status.render();
    for secret in raw {
        assert!(!shown.contains(secret), "status shows {secret}");
    }
}

#[test]
fn overlapping_and_repeated_matches_are_withheld_whole() {
    let secrets = secrets();
    assert_eq!(
        secrets.redact("a SECRET-123456-TAIL b").as_deref(),
        Some("a <withheld: secret> b")
    );
    assert_eq!(
        secrets.redact("SECRET-000001SECRET-000002").as_deref(),
        Some("<withheld: secret>")
    );
    assert_eq!(
        secrets.redact("x tok_abcdefgh y tok_hgfedcba z").as_deref(),
        Some("x <withheld: secret> y <withheld: secret> z")
    );
    assert_eq!(
        secrets.redact("ключ-абвг→ok").as_deref(),
        Some("<withheld: secret>→ok")
    );
    assert_eq!(secrets.redact("nothing here, 日本"), None);
}

#[test]
fn an_invalid_secret_pattern_stops_the_snapshot_without_a_panic_or_a_leak() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let mut options = test_options();
    options.secret_patterns.push("(unclosed".to_string());
    repo.set_backup_options(options);
    writer(&repo)
        .transact(|tx| {
            tx.set_task(
                "l1",
                TaskStateRecord {
                    branch: Some("SECRET-123123".to_string()),
                    ..TaskStateRecord::default()
                },
            )
        })
        .unwrap();

    let error = block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap_err();
    assert!(format!("{error:#}").contains("(unclosed"), "{error:#}");
    let repository = &repo.location().repository;
    assert!(
        block_on(state_branch::tip(
            repo.git(),
            repository,
            "refs/heads/bongos/state"
        ))
        .is_none()
    );
    let status = block_on(repo.backup_status(start())).unwrap();
    let error = status.snapshot_error.unwrap();
    assert!(error.contains("no snapshot is taken"), "{error}");
    assert!(!error.contains("SECRET-123123"));
}

#[test]
fn exporter_paths_are_confined_to_the_exporters_namespace() {
    for (path, refused) in [
        ("notes/a.toml", false),
        ("notes", false),
        ("notes/deep/er/file.txt", false),
        ("../notes/a", true),
        ("notes/../tasks/x.toml", true),
        ("/etc/passwd", true),
        ("notes/.git/config", true),
        ("notes/.gitattributes", true),
        ("other/a.toml", true),
        ("tasks/l1.toml", true),
        ("FORMAT", true),
        ("", true),
        ("notes//a", true),
        ("notes/a\nb", true),
        ("notes\\..\\x", true),
    ] {
        assert_eq!(
            exported_path_problem("notes", path).is_some(),
            refused,
            "{path:?}"
        );
    }
    for (namespace, refused) in [
        ("notes", false),
        ("tasks", true),
        ("Tasks", true),
        ("FORMAT", true),
        ("runs.toml", true),
        ("withheld.toml", true),
        (".git", true),
        ("", true),
        ("..", true),
        ("a/b", true),
    ] {
        assert_eq!(
            namespace_problem(namespace).is_some(),
            refused,
            "{namespace:?}"
        );
    }
}

#[test]
fn a_bad_exporter_is_reported_and_the_rest_of_the_snapshot_is_whole() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    repo.use_exporters(exporters(vec![
        fixed(
            "good",
            &[("good/a.toml", "x = 1\n"), ("good/a.toml", "x = 2\n")],
        ),
        fixed("tasks", &[("tasks/l1.toml", "status = \"closed\"\n")]),
        fixed("good", &[("good/b.toml", "y = 1\n")]),
        fixed(
            "stray",
            &[("good/c.toml", "z = 1\n"), ("stray/.git/hooks", "boom")],
        ),
        fixed("", &[]),
        Arc::new(Failing),
        Arc::new(Panicking),
        fixed("last", &[("last/ok.txt", "fine")]),
    ]));

    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();

    let files = state_files(&repo);
    assert_eq!(files["good/a.toml"], "x = 1\n");
    assert_eq!(files["last/ok.txt"], "fine");
    assert!(!files.contains_key("good/b.toml") && !files.contains_key("good/c.toml"));
    assert!(files.keys().all(|path| !path.contains(".git")));
    let task: TaskStateRecord = toml::from_str(&files["tasks/l1.toml"]).unwrap();
    assert_eq!(task.status, Status::Open);

    let status = block_on(repo.backup_status(start())).unwrap();
    let problems = status.problems.join("\n");
    for expected in [
        "given twice",
        "exporter tasks: the namespace is reserved",
        "another exporter has this namespace",
        "outside the exporter's namespace",
        "names a git file",
        "exporter : the namespace is not a valid file name",
        "exporter failing failed",
        "exporter panicking panicked",
    ] {
        assert!(
            problems.contains(expected),
            "{expected:?} missing from\n{problems}"
        );
    }
    assert!(!problems.contains("SECRET-424242"), "{problems}");
}

/// Writes to the store from inside its own export, through the writer, while the export reads
/// through a reader: the snapshot shows the store as it was when the export began.
struct WritesDuringExport(Arc<StateWriter>);

impl StateExporter for WritesDuringExport {
    fn namespace(&self) -> &str {
        "probe"
    }

    fn export(&self, read: &StateRead) -> Result<Vec<(RelPathBuf, String)>> {
        self.0
            .transact(|tx| tx.set_task("l2", task_with_todos(&[("r1", TodoState::Done)])))?;
        let seen = read.task("l2")?.is_some();
        Ok(vec![(path("probe/seen.txt"), seen.to_string())])
    }

    fn import(&self, _tx: &mut StateTx, _files: &[(RelPathBuf, String)]) -> Result<()> {
        Ok(())
    }
}

#[test]
fn a_snapshot_is_one_consistent_generation_while_writes_go_on() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    let writer = writer(&repo);
    let before = writer.generation().unwrap();
    let reader = StateStore::open_reader(repo.location()).unwrap();
    let exporters: Vec<Arc<dyn StateExporter>> = vec![Arc::new(WritesDuringExport(writer.clone()))];

    let exported = reader
        .read(|read| snapshot::export(read, &exporters, &secrets()))
        .unwrap();

    assert_eq!(exported.generation, before);
    assert_eq!(writer.generation().unwrap(), before + 1);
    assert_eq!(exported.files["probe/seen.txt"], "false");
    assert!(exported.files.contains_key("tasks/l1.toml"));
    assert!(!exported.files.contains_key("tasks/l2.toml"));
}

fn main_history_paths(repo: &ProjectRepo) -> String {
    block_on(repo.git().run(
        &repo.location().repository,
        &["log", "main", "--name-only", "--format=%H"],
    ))
    .unwrap()
}

#[test]
fn snapshots_never_reach_master_or_a_working_copy() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    let main_before = block_on(repo.git().rev_parse(&repo.location().repository, "main")).unwrap();

    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
    change_state(&repo, TodoState::Open);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start() + chrono::Duration::hours(1)))
        .unwrap();

    let repository = &repo.location().repository;
    assert_eq!(
        block_on(repo.git().rev_parse(repository, "main")).unwrap(),
        main_before
    );
    let history = main_history_paths(&repo);
    assert!(
        !history.contains("FORMAT") && !history.contains("withheld.toml"),
        "{history}"
    );
    // The checkout of master and a working copy made from it have none of the state files.
    for checkout in [repository.clone(), root.path().join("copy")] {
        if checkout != *repository {
            block_on(
                repo.git()
                    .worktree_add(repository, &checkout, "work/copy", "main"),
            )
            .unwrap();
        }
        for name in ["FORMAT", "tasks", "runs.toml", "withheld.toml"] {
            assert!(
                !checkout.join(name).exists(),
                "{}",
                checkout.join(name).display()
            );
        }
    }
    // The state branch shares no history with master and has no definitions.
    let files = state_files(&repo);
    assert!(files.keys().all(|path| !path.starts_with("definitions/")));
    assert!(!block_on(repo.git().is_ancestor(
        repository,
        "main",
        "bongos/state"
    )));
    assert!(!block_on(repo.git().is_ancestor(
        repository,
        "bongos/state",
        "main"
    )));
    // The temporary index and files are gone.
    assert!(!repo.location().state.join(SNAPSHOT_INDEX_FILE).exists());
    assert!(!repo.location().state.join("snapshot.files").exists());
    // The repository's own index was never touched.
    assert!(block_on(repo.git().is_clean(repository)).unwrap());
}

#[test]
fn a_snapshot_that_loses_the_race_for_the_branch_changes_nothing() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
    let git = repo.git();
    let repository = &repo.location().repository;
    let reference = "refs/heads/bongos/state";
    let first = block_on(state_branch::tip(git, repository, reference)).unwrap();
    change_state(&repo, TodoState::Open);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start() + chrono::Duration::hours(1)))
        .unwrap();
    let second = block_on(state_branch::tip(git, repository, reference)).unwrap();

    let mut files = BTreeMap::new();
    files.insert("FORMAT".to_string(), "bongos-state 1\n".to_string());
    let committed = block_on(state_branch::commit_snapshot(
        git,
        repository,
        &repo.location().state,
        reference,
        &files,
        Some(&first),
        "stale",
    ))
    .unwrap();

    assert_eq!(committed, Committed::LostRace);
    assert_eq!(
        block_on(state_branch::tip(git, repository, reference)).unwrap(),
        second
    );
    assert!(!repo.location().state.join(SNAPSHOT_INDEX_FILE).exists());
}

#[test]
fn the_state_branch_must_be_a_safe_branch_that_is_not_master() {
    assert_eq!(
        state_ref("bongos/state").unwrap(),
        "refs/heads/bongos/state"
    );
    assert!(state_ref("backup-1").is_ok());
    for name in [
        "main",
        "Main",
        "master",
        "main/state",
        "master/x",
        "refs/heads/main",
        "refs/heads/bongos/state",
        "HEAD",
        "-f",
        "--force",
        "a..b",
        "a/.hidden",
        "a.lock",
        "a b",
        "a@{1}",
        "",
        "/abs",
        "trailing/",
        "a//b",
    ] {
        assert!(state_ref(name).is_err(), "{name:?}");
    }
}

#[test]
fn a_bad_state_branch_setting_is_reported_and_nothing_is_written() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    change_state(&repo, TodoState::Done);
    let mut options = test_options();
    options.state_branch = "main".to_string();
    repo.set_backup_options(options);

    assert!(block_on(repo.snapshot_now(SnapshotReason::Manual, start())).is_err());
    let history = main_history_paths(&repo);
    assert!(!history.contains("FORMAT"));
    let status = block_on(repo.backup_status(start())).unwrap();
    assert!(status.snapshot_error.unwrap().contains("would be master"));
}
