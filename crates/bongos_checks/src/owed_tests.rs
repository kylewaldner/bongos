//! Tests of the owed markers and the `checks` hold: the `checks-owed` hook marks a leaf in the
//! transaction that finishes it, deletes the marker and the done record in the one that reopens
//! it (with the setting on or off), changes nothing for a task that stays finished, and aborts the
//! transaction when it fails; the hold is "marker and no valid done record" in any process that
//! reads the store.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bongos_project_repo::{
    StateApplyHooks, StateEvent, StateHandle, StateOp, StateStore, StateWriter,
};
use bongos_project_repo_model::{DefinitionTree, RelPath, TaskStateRecord, TodoStateRecord};
use bongos_work_model::{ResultKind, ResultState, Status, TaskResult, TodoState};
use futures::StreamExt as _;
use futures::channel::mpsc;
use tempfile::TempDir;

use crate::evidence_store::{
    DONE_NAMESPACE, DoneRecord, LAST_NAMESPACE, OWED_NAMESPACE, SEQ_KEY, SEQ_NAMESPACE, put_done,
};
use crate::holds::{CHECKING, ChecksHolds};
use crate::live::{HoldRow, hold_key};
use crate::owed::{ChecksOwedHook, FolderSets, LastChild, LastRow, OwedMarker, put_marker};
use crate::test_support::{PROJECT_TOML, location};
use crate::{LIVE_NAMESPACE, read_holds};

const PROJECT: &str = "project-1";

const TESTS: &str = "[[checks.check]]\nname = \"tests\"\ncommand = [\"make\", \"test\"]\n";
const OPTIONAL: &str =
    "[[checks.check]]\nname = \"lint\"\ncommand = [\"make\", \"lint\"]\nrequired = false\n";
const RUST_SET: &str = "[[check]]\nname = \"clippy\"\ncommand = [\"cargo\", \"clippy\"]\n";

/// A task's `task.toml`: two todos, `a` and `b`, then `checks`.
fn task_toml(id: &str, kind: &str, todos: &[&str], checks: &str) -> String {
    let todos: String = todos
        .iter()
        .map(|todo| format!("[[todos]]\nid = \"{todo}\"\nname = \"{todo}\"\n\n"))
        .collect();
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{id}\"\nkind = \"{kind}\"\n\
         created = 2026-01-01T10:00:00Z\n\n{todos}{checks}\n"
    )
}

/// A tree from `(folder, task.toml text)`, below the top group `g1`.
fn tree_of(tasks: &[(&str, String)]) -> DefinitionTree {
    let mut files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            PROJECT_TOML.as_bytes().to_vec(),
        ),
        (
            RelPath::new("task.toml").unwrap(),
            task_toml("g1", "other", &[], "").into_bytes(),
        ),
    ];
    for (folder, text) in tasks {
        files.push((
            RelPath::new(format!("{folder}/task.toml")).unwrap(),
            text.clone().into_bytes(),
        ));
    }
    DefinitionTree::parse_files(files).unwrap()
}

/// The leaves the table runs on.
fn leaves() -> DefinitionTree {
    tree_of(&[
        (
            "tasks/plain",
            task_toml("plain", "other", &["a", "b"], TESTS),
        ),
        ("tasks/bare", task_toml("bare", "other", &["a", "b"], "")),
        (
            "tasks/doc",
            task_toml("doc", "document", &["a", "b"], TESTS),
        ),
        (
            "tasks/optional",
            task_toml("optional", "other", &["a", "b"], OPTIONAL),
        ),
        (
            "tasks/setuser",
            task_toml(
                "setuser",
                "other",
                &["a", "b"],
                "[checks]\nuse = [\"rust\"]\n",
            ),
        ),
        (
            "tasks/unknown",
            task_toml(
                "unknown",
                "other",
                &["a", "b"],
                "[checks]\nuse = [\"missing\"]\n",
            ),
        ),
    ])
}

fn record(status: Status, todos: [TodoState; 2]) -> TaskStateRecord {
    TaskStateRecord {
        status,
        todos: ["a", "b"]
            .into_iter()
            .zip(todos)
            .map(|(id, state)| TodoStateRecord {
                id: Some(id.to_string()),
                state,
                started: None,
            })
            .collect(),
        ..TaskStateRecord::default()
    }
}

fn last_open() -> TaskStateRecord {
    record(Status::Open, [TodoState::Done, TodoState::Open])
}

fn closed() -> TaskStateRecord {
    record(Status::Open, [TodoState::Done, TodoState::Done])
}

fn skipped() -> TaskStateRecord {
    record(Status::Open, [TodoState::Done, TodoState::Skipped])
}

fn completed() -> TaskStateRecord {
    record(Status::Done, [TodoState::Done, TodoState::Open])
}

fn cancelled() -> TaskStateRecord {
    record(Status::Cancelled, [TodoState::Done, TodoState::Open])
}

/// A record with the todos `(id, state)`.
fn todos_record(status: Status, todos: &[(&str, TodoState)]) -> TaskStateRecord {
    TaskStateRecord {
        status,
        todos: todos
            .iter()
            .map(|(id, state)| TodoStateRecord {
                id: Some(id.to_string()),
                state: *state,
                started: None,
            })
            .collect(),
        ..TaskStateRecord::default()
    }
}

fn merged(mut record: TaskStateRecord) -> TaskStateRecord {
    record.result = Some(TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://example.com/pr/1".to_string()),
        state: Some(ResultState::Merged),
        urls: Vec::new(),
        extra: Default::default(),
    });
    record
}

fn done_record() -> DoneRecord {
    DoneRecord {
        tree_key: "tree1".to_string(),
        generation: 1,
        at: "2026-01-01T10:00:00Z".to_string(),
        evidence: vec!["r1".to_string()],
    }
}

/// A project's state store with the hook registered on its own registry (never the global one),
/// its named sets in a folder of the fixture.
struct Fixture {
    root: TempDir,
    hook: Arc<ChecksOwedHook>,
    definitions: Arc<DefinitionTree>,
    writer: Option<Arc<StateWriter>>,
}

impl Fixture {
    fn new(enabled: bool) -> Self {
        Self::with(enabled, leaves())
    }

    fn with(enabled: bool, definitions: DefinitionTree) -> Self {
        let root = TempDir::new().unwrap();
        let sets_dir = root.path().join("definitions");
        let sets = sets_dir.join("checks").join("sets");
        std::fs::create_dir_all(&sets).unwrap();
        std::fs::write(sets.join("rust.toml"), RUST_SET).unwrap();
        let hook = Arc::new(ChecksOwedHook::new());
        hook.set_enabled(enabled);
        hook.set_named_sets(Arc::new(FolderSets(sets_dir)));
        let mut fixture = Self {
            root,
            hook,
            definitions: Arc::new(definitions),
            writer: None,
        };
        fixture.open();
        fixture
    }

    fn open(&mut self) {
        let hooks = StateApplyHooks::new();
        hooks.register(self.hook.clone());
        let writer = StateStore::open_writer_with_hooks(&location(&self.root), &hooks).unwrap();
        writer.set_definitions(self.definitions.clone());
        self.writer = Some(Arc::new(writer));
    }

    /// Closes the store as a crash or a quit would, and opens it again. Nothing runs at start.
    fn restart(&mut self) {
        self.writer = None;
        self.open();
    }

    fn writer(&self) -> &Arc<StateWriter> {
        self.writer.as_ref().unwrap()
    }

    fn set(&self, id: &str, state: TaskStateRecord) -> anyhow::Result<()> {
        self.writer().transact(|tx| tx.set_task(id, state))
    }

    /// Sets `id`'s state and returns the event of that one transaction.
    fn set_with_event(&self, id: &str, state: TaskStateRecord) -> (Vec<String>, Vec<String>) {
        let mut events = self.writer().subscribe();
        self.set(id, state).unwrap();
        let Ok(StateEvent::Changed {
            task_ids,
            namespaces,
            ..
        }) = events.try_recv()
        else {
            panic!("the write sent no event");
        };
        assert!(events.try_recv().is_err(), "one transaction, one event");
        (task_ids, namespaces)
    }

    /// Whether `id` has a marker and a done record.
    fn rows(&self, id: &str) -> (bool, bool) {
        self.writer()
            .read(|read| {
                Ok((
                    read.kv(OWED_NAMESPACE).get(id)?.is_some(),
                    read.kv(DONE_NAMESPACE).get(id)?.is_some(),
                ))
            })
            .unwrap()
    }

    /// Puts or deletes the marker and the done record of `id` in a transaction that writes no
    /// task row, so no hook runs.
    fn seed(&self, id: &str, (marker, done): (bool, bool)) {
        self.writer()
            .transact(|tx| {
                if marker {
                    put_marker(tx, id, &OwedMarker::default())?;
                } else {
                    tx.kv(OWED_NAMESPACE).delete(id)?;
                }
                if done {
                    put_done(tx, id, &done_record())?;
                } else {
                    tx.kv(DONE_NAMESPACE).delete(id)?;
                }
                Ok(())
            })
            .unwrap();
    }

    /// Lands `definitions` as a new verified commit: every task whose definition differs is
    /// touched.
    fn land(&mut self, definitions: DefinitionTree) {
        let definitions = Arc::new(definitions);
        self.writer()
            .transact(|tx| tx.set_verified_commit("c2", definitions.clone()))
            .unwrap();
        self.definitions = definitions;
    }

    /// Lands `definitions` and applies `ops` in one transaction, as a landed change does.
    fn land_with(&mut self, definitions: DefinitionTree, ops: Vec<StateOp>) {
        let definitions = Arc::new(definitions);
        self.writer()
            .transact(|tx| {
                tx.set_verified_commit("c2", definitions.clone())?;
                tx.apply_ops(&ops)?;
                Ok(())
            })
            .unwrap();
        self.definitions = definitions;
    }

    fn holds(&self) -> std::collections::BTreeMap<String, String> {
        self.writer().read(read_holds).unwrap()
    }
}

#[test]
fn which_transitions_mark_and_which_delete() {
    struct Case {
        name: &'static str,
        task: &'static str,
        enabled: bool,
        from: TaskStateRecord,
        /// The marker and done record put before the write.
        seeded: (bool, bool),
        to: TaskStateRecord,
        /// The marker and done record after it.
        expected: (bool, bool),
    }
    let none = (false, false);
    let marked = (true, false);
    let both = (true, true);
    let cases = [
        Case {
            name: "the last open todo closes",
            task: "plain",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: marked,
        },
        Case {
            name: "the last open todo is skipped",
            task: "plain",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: skipped(),
            expected: marked,
        },
        Case {
            name: "the status is set to Done (complete, close)",
            task: "plain",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: completed(),
            expected: marked,
        },
        Case {
            name: "finished while the setting is off",
            task: "plain",
            enabled: false,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: none,
        },
        Case {
            name: "a task with no checks",
            task: "bare",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: none,
        },
        Case {
            name: "a document task with a checks table",
            task: "doc",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: none,
        },
        Case {
            name: "a task whose only check is optional",
            task: "optional",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: none,
        },
        Case {
            name: "a required check from a named set",
            task: "setuser",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: marked,
        },
        Case {
            name: "a named set that cannot be read",
            task: "unknown",
            enabled: true,
            from: last_open(),
            seeded: none,
            to: closed(),
            expected: marked,
        },
        Case {
            name: "a stale done record goes when the task is marked",
            task: "plain",
            enabled: true,
            from: last_open(),
            seeded: (false, true),
            to: closed(),
            expected: marked,
        },
        Case {
            name: "a task that stays finished keeps its rows",
            task: "plain",
            enabled: true,
            from: closed(),
            seeded: both,
            to: completed(),
            expected: both,
        },
        Case {
            name: "a task that stays finished without a marker gets none",
            task: "plain",
            enabled: true,
            from: closed(),
            seeded: none,
            to: completed(),
            expected: none,
        },
        Case {
            name: "a reopen while on deletes both rows",
            task: "plain",
            enabled: true,
            from: closed(),
            seeded: both,
            to: last_open(),
            expected: none,
        },
        Case {
            name: "a reopen while off deletes both rows",
            task: "plain",
            enabled: false,
            from: closed(),
            seeded: both,
            to: last_open(),
            expected: none,
        },
        Case {
            name: "a cancel deletes both rows",
            task: "plain",
            enabled: true,
            from: closed(),
            seeded: both,
            to: cancelled(),
            expected: none,
        },
    ];
    for case in cases {
        let fixture = Fixture::new(case.enabled);
        fixture.set(case.task, case.from).unwrap();
        fixture.seed(case.task, case.seeded);
        fixture.set(case.task, case.to).unwrap();
        assert_eq!(fixture.rows(case.task), case.expected, "{}", case.name);
    }
}

#[test]
fn closing_the_last_todo_commits_the_marker_in_the_same_transaction_and_writes_only_kv() {
    let fixture = Fixture::new(true);
    // A write that finishes nothing names no checks namespace.
    let (task_ids, namespaces) = fixture.set_with_event("plain", last_open());
    assert_eq!(task_ids, ["plain"]);
    assert!(namespaces.is_empty(), "{namespaces:?}");
    let generation = fixture.writer().generation().unwrap();

    let (task_ids, namespaces) = fixture.set_with_event("plain", closed());
    assert_eq!(task_ids, ["plain"]);
    assert_eq!(namespaces, [OWED_NAMESPACE]);
    assert_eq!(fixture.writer().generation().unwrap(), generation + 1);
    let marker = fixture
        .writer()
        .read(|read| read.kv(OWED_NAMESPACE).get("plain"))
        .unwrap()
        .unwrap();
    assert_eq!(
        marker.try_into::<OwedMarker>().unwrap(),
        OwedMarker::default()
    );
    assert_eq!(
        fixture.holds().get("plain").map(String::as_str),
        Some(CHECKING)
    );
}

#[test]
fn a_failing_hook_aborts_the_whole_transaction() {
    let fixture = Fixture::new(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.hook.fail.store(true, Ordering::SeqCst);
    let error = fixture.set("plain", closed()).unwrap_err();
    assert!(format!("{error:#}").contains("checks-owed"), "{error:#}");
    let state = fixture
        .writer()
        .read(|read| read.task("plain"))
        .unwrap()
        .unwrap();
    assert_eq!(
        state.todos[1].state,
        TodoState::Open,
        "the close did not commit"
    );
    assert_eq!(fixture.rows("plain"), (false, false));

    fixture.hook.fail.store(false, Ordering::SeqCst);
    fixture.set("plain", closed()).unwrap();
    assert_eq!(fixture.rows("plain"), (true, false));
}

#[test]
fn a_crash_after_the_closing_write_leaves_the_task_held() {
    let mut fixture = Fixture::new(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    fixture.restart();
    assert_eq!(fixture.rows("plain"), (true, false));
    let reader = StateStore::open_reader(&location(&fixture.root)).unwrap();
    let holds = reader.read(read_holds).unwrap();
    assert_eq!(holds.get("plain").map(String::as_str), Some(CHECKING));
}

#[test]
fn restarts_and_turning_the_setting_off_and_on_change_nothing() {
    let mut fixture = Fixture::new(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    // Finished while off: stays unmarked through everything below.
    fixture.hook.set_enabled(false);
    fixture.set("bare", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    fixture.set("setuser", last_open()).unwrap();
    fixture.set("setuser", closed()).unwrap();
    fixture.hook.set_enabled(true);
    let held = fixture.holds();
    assert_eq!(held.keys().collect::<Vec<_>>(), ["plain"]);

    fixture.restart();
    fixture.restart();
    assert_eq!(fixture.holds(), held);

    for enabled in [false, true] {
        fixture.hook.set_enabled(enabled);
        // Writes to tasks that stay finished.
        fixture.set("plain", completed()).unwrap();
        fixture.set("setuser", completed()).unwrap();
        assert_eq!(fixture.holds(), held, "setting {enabled}");
        assert_eq!(fixture.rows("setuser"), (false, false));
    }
}

#[test]
fn a_reopen_deletes_both_rows_in_its_own_transaction_even_while_off() {
    let fixture = Fixture::new(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    fixture.seed("plain", (true, true));
    assert!(
        fixture.holds().is_empty(),
        "a valid done record clears the hold"
    );

    fixture.hook.set_enabled(false);
    let (task_ids, mut namespaces) = fixture.set_with_event("plain", last_open());
    namespaces.sort();
    assert_eq!(task_ids, ["plain"]);
    assert_eq!(namespaces, [DONE_NAMESPACE, OWED_NAMESPACE]);
    assert_eq!(fixture.rows("plain"), (false, false));

    // Closed again while off: not marked, so not held when the setting comes back.
    fixture.set("plain", closed()).unwrap();
    assert_eq!(fixture.rows("plain"), (false, false));

    fixture.hook.set_enabled(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    assert_eq!(fixture.rows("plain"), (true, false));
    assert_eq!(
        fixture.holds().get("plain").map(String::as_str),
        Some(CHECKING)
    );
}

#[test]
fn a_landed_change_that_removes_the_last_open_todo_marks_the_task() {
    for (enabled, expected) in [(true, (true, false)), (false, (false, false))] {
        let mut fixture = Fixture::new(enabled);
        fixture.set("plain", last_open()).unwrap();
        let mut tasks: Vec<(&str, String)> =
            vec![("tasks/plain", task_toml("plain", "other", &["a"], TESTS))];
        tasks.push(("tasks/bare", task_toml("bare", "other", &["a", "b"], "")));
        fixture.land(tree_of(&tasks));
        assert_eq!(fixture.rows("plain"), expected, "setting {enabled}");
    }
}

#[test]
fn landed_changes_that_move_todos_or_work_or_delete_a_task() {
    struct Case {
        name: &'static str,
        before: Vec<(&'static str, String)>,
        after: Vec<(&'static str, String)>,
        /// Each task's state, then its seeded marker and done record.
        states: Vec<(&'static str, TaskStateRecord, (bool, bool))>,
        ops: Vec<StateOp>,
        /// The marker and done record of each task afterward.
        expected: Vec<(&'static str, (bool, bool))>,
    }
    let move_todo = |from: &str, to: &str, todo: &str| StateOp::MoveTodo {
        from_task: from.to_string(),
        to_task: to.to_string(),
        todo_id: todo.to_string(),
    };
    let (open, done) = (TodoState::Open, TodoState::Done);
    let cases = vec![
        Case {
            name: "the last open todo moved away finishes the source; the target reopens",
            before: vec![
                (
                    "tasks/plain",
                    task_toml("plain", "other", &["a", "b"], TESTS),
                ),
                ("tasks/sink", task_toml("sink", "other", &["x"], TESTS)),
            ],
            after: vec![
                ("tasks/plain", task_toml("plain", "other", &["a"], TESTS)),
                ("tasks/sink", task_toml("sink", "other", &["x", "b"], TESTS)),
            ],
            states: vec![
                ("plain", last_open(), (false, false)),
                (
                    "sink",
                    todos_record(Status::Open, &[("x", done)]),
                    (true, true),
                ),
            ],
            ops: vec![move_todo("plain", "sink", "b")],
            expected: vec![("plain", (true, false)), ("sink", (false, false))],
        },
        Case {
            name: "a split turns the finished leaf into a group of new leaves",
            before: vec![(
                "tasks/plain",
                task_toml("plain", "other", &["a", "b"], TESTS),
            )],
            after: vec![
                ("tasks/plain", task_toml("plain", "other", &[], TESTS)),
                (
                    "tasks/plain/tasks/one",
                    task_toml("one", "other", &["a"], TESTS),
                ),
                (
                    "tasks/plain/tasks/two",
                    task_toml("two", "other", &["b"], TESTS),
                ),
            ],
            states: vec![("plain", closed(), (true, false))],
            ops: vec![
                move_todo("plain", "one", "a"),
                move_todo("plain", "two", "b"),
            ],
            expected: vec![
                ("plain", (false, false)),
                ("one", (true, false)),
                ("two", (true, false)),
            ],
        },
        Case {
            name: "moving the merged work away reopens a merge task and finishes the target",
            before: vec![
                ("tasks/pr", task_toml("pr", "pr-merge", &["a"], TESTS)),
                ("tasks/next", task_toml("next", "pr-merge", &["a"], TESTS)),
            ],
            after: vec![
                ("tasks/pr", task_toml("pr", "pr-merge", &["a"], TESTS)),
                ("tasks/next", task_toml("next", "pr-merge", &["a"], TESTS)),
            ],
            states: vec![
                (
                    "pr",
                    merged(todos_record(Status::Open, &[("a", open)])),
                    (true, true),
                ),
                (
                    "next",
                    todos_record(Status::Open, &[("a", open)]),
                    (false, false),
                ),
            ],
            ops: vec![StateOp::MoveWork {
                from_task: "pr".to_string(),
                to_task: "next".to_string(),
            }],
            expected: vec![("pr", (false, false)), ("next", (true, false))],
        },
        Case {
            name: "a task deleted from the definitions loses its rows",
            before: vec![
                (
                    "tasks/plain",
                    task_toml("plain", "other", &["a", "b"], TESTS),
                ),
                ("tasks/bare", task_toml("bare", "other", &["a"], "")),
            ],
            after: vec![("tasks/bare", task_toml("bare", "other", &["a"], ""))],
            states: vec![("plain", closed(), (true, true))],
            ops: Vec::new(),
            expected: vec![("plain", (false, false))],
        },
    ];
    for case in cases {
        let mut fixture = Fixture::with(true, tree_of(&case.before));
        for (task, state, seeded) in case.states {
            fixture.set(task, state).unwrap();
            fixture.seed(task, seeded);
        }
        fixture.land_with(tree_of(&case.after), case.ops);
        for (task, expected) in case.expected {
            assert_eq!(fixture.rows(task), expected, "{}: {task}", case.name);
        }
    }
}

#[test]
fn a_leaf_that_gains_children_loses_its_rows_even_when_its_file_is_unchanged() {
    let leaf = task_toml("plain", "other", &[], TESTS);
    let mut fixture = Fixture::with(true, tree_of(&[("tasks/plain", leaf.clone())]));
    fixture.set("plain", completed()).unwrap();
    fixture.seed("plain", (true, true));
    fixture.land(tree_of(&[
        ("tasks/plain", leaf),
        (
            "tasks/plain/tasks/child",
            task_toml("child", "other", &["a"], TESTS),
        ),
    ]));
    assert_eq!(fixture.rows("plain"), (false, false));
}

#[test]
fn before_the_definitions_load_only_a_cancel_or_a_deleted_row_deletes() {
    let fixture = Fixture::with(true, DefinitionTree::default());
    // Without the definition an open todo may be one it dropped, so no todo or status change
    // alone deletes.
    let cases = [
        ("a todo reopened", closed(), last_open(), (true, true)),
        (
            "the status set back from Done",
            completed(),
            last_open(),
            (true, true),
        ),
        ("a cancel", closed(), cancelled(), (false, false)),
        (
            "a task that stays finished",
            closed(),
            completed(),
            (true, true),
        ),
        (
            "an open todo that was open before",
            last_open(),
            record(Status::Open, [TodoState::Skipped, TodoState::Open]),
            (true, true),
        ),
        (
            "a reopened todo on a merged task",
            merged(closed()),
            merged(last_open()),
            (true, true),
        ),
    ];
    for (name, from, to, expected) in cases {
        fixture.set("plain", from).unwrap();
        assert_eq!(fixture.rows("plain"), (false, false), "{name}: not marked");
        fixture.seed("plain", (true, true));
        fixture.set("plain", to).unwrap();
        assert_eq!(fixture.rows("plain"), expected, "{name}");
        fixture.seed("plain", (false, false));
    }
    fixture.seed("plain", (true, true));
    fixture
        .writer()
        .transact(|tx| {
            tx.apply_ops(&[StateOp::Delete {
                task: "plain".to_string(),
            }])
        })
        .unwrap();
    assert_eq!(fixture.rows("plain"), (false, false), "a deleted state row");
}

#[test]
fn a_check_added_later_to_a_finished_task_leaves_it_done() {
    let late = |kind: &str, checks: &str| task_toml("late", kind, &["a", "b"], checks);
    let checked_group = task_toml("checked", "other", &[], TESTS);
    let cases: [(&str, Vec<(&str, String)>, Vec<(&str, String)>); 4] = [
        (
            "its own checks table",
            vec![("tasks/late", late("other", ""))],
            vec![("tasks/late", late("other", TESTS))],
        ),
        (
            "a named set",
            vec![("tasks/late", late("other", ""))],
            vec![("tasks/late", late("other", "[checks]\nuse = [\"rust\"]\n"))],
        ),
        (
            "a move under a parent with checks",
            vec![
                ("tasks/late", late("other", "")),
                ("tasks/checked", checked_group.clone()),
                (
                    "tasks/checked/tasks/first",
                    task_toml("first", "other", &["a"], ""),
                ),
            ],
            vec![
                ("tasks/checked", checked_group),
                (
                    "tasks/checked/tasks/first",
                    task_toml("first", "other", &["a"], ""),
                ),
                ("tasks/checked/tasks/late", late("other", "")),
            ],
        ),
        (
            "a kind change from document",
            vec![("tasks/late", late("document", TESTS))],
            vec![("tasks/late", late("other", TESTS))],
        ),
    ];
    for (name, before, after) in cases {
        let mut fixture = Fixture::with(true, tree_of(&before));
        fixture.set("late", last_open()).unwrap();
        fixture.set("late", closed()).unwrap();
        assert_eq!(fixture.rows("late"), (false, false), "{name}: before");
        fixture.land(tree_of(&after));
        assert_eq!(fixture.rows("late"), (false, false), "{name}");
        assert!(fixture.holds().is_empty(), "{name}");
    }
}

#[test]
fn a_reader_holds_the_task_at_once_and_follows_its_hold_row() {
    let fixture = Fixture::new(true);
    let reader = Arc::new(StateStore::open_reader(&location(&fixture.root)).unwrap());
    let (follow, _followed) = mpsc::unbounded();
    let holds = ChecksHolds::new(Default::default(), follow);
    let handles = [(PROJECT.to_string(), StateHandle::Reader(reader))];
    assert!(!holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain"), None);

    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    assert!(holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain").as_deref(), Some(CHECKING));

    let reason = "not done: 1 check missing (tests)";
    fixture
        .writer()
        .transact(|tx| {
            tx.kv(LIVE_NAMESPACE).put(
                &hold_key("plain"),
                toml::Value::try_from(HoldRow {
                    reason: reason.to_string(),
                })?,
            )
        })
        .unwrap();
    assert!(holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain").as_deref(), Some(reason));

    fixture
        .writer()
        .transact(|tx| put_done(tx, "plain", &done_record()))
        .unwrap();
    assert!(holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain"), None);
    assert!(!holds.refresh_from(&handles));
}

#[test]
fn an_older_read_applied_after_a_newer_one_never_drops_a_hold() {
    let fixture = Fixture::new(true);
    let reader = Arc::new(StateStore::open_reader(&location(&fixture.root)).unwrap());
    let (follow, _followed) = mpsc::unbounded();
    let holds = ChecksHolds::new(Default::default(), follow);
    let handles = [(PROJECT.to_string(), StateHandle::Reader(reader))];
    fixture.set("plain", last_open()).unwrap();

    // Two refreshes run at once: the first reads before the task finishes, the second after,
    // and the first one's swap comes last.
    let older = holds.read_from(holds.begin_read(), &handles);
    fixture.set("plain", closed()).unwrap();
    let newer = holds.read_from(holds.begin_read(), &handles);
    assert!(holds.apply(newer));
    assert_eq!(holds.hold("plain").as_deref(), Some(CHECKING));
    assert!(!holds.apply(older), "the older read is dropped");
    assert_eq!(holds.hold("plain").as_deref(), Some(CHECKING));

    // A later read still replaces it.
    fixture
        .writer()
        .transact(|tx| put_done(tx, "plain", &done_record()))
        .unwrap();
    assert!(holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain"), None);
}

#[test]
fn a_read_that_listed_before_a_project_opened_never_drops_its_holds() {
    let fixture = Fixture::new(true);
    let reader = Arc::new(StateStore::open_reader(&location(&fixture.root)).unwrap());
    let (follow, _followed) = mpsc::unbounded();
    let holds = ChecksHolds::new(Default::default(), follow);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();

    // A slow refresh begins and lists no project; the project opens and a second refresh reads
    // it and applies first. The slow one's read, numbered when it began, comes last.
    let slow_number = holds.begin_read();
    let handles = [(PROJECT.to_string(), StateHandle::Reader(reader))];
    assert!(holds.refresh_from(&handles));
    assert_eq!(holds.hold("plain").as_deref(), Some(CHECKING));
    let slow = holds.read_from(slow_number, &[]);
    assert!(
        !holds.apply(slow),
        "the read that missed the project is dropped"
    );
    assert_eq!(holds.hold("plain").as_deref(), Some(CHECKING));
}

#[test]
fn an_unreadable_done_record_does_not_clear_the_hold() {
    let fixture = Fixture::new(true);
    fixture.set("plain", last_open()).unwrap();
    fixture.set("plain", closed()).unwrap();
    fixture
        .writer()
        .transact(|tx| {
            tx.kv(DONE_NAMESPACE)
                .put("plain", toml::Value::String("passed".to_string()))
        })
        .unwrap();
    assert_eq!(
        fixture.holds().get("plain").map(String::as_str),
        Some(CHECKING)
    );
}

#[test]
fn the_writer_s_events_are_followed_once() {
    let fixture = Fixture::new(true);
    let (follow, mut followed) = mpsc::unbounded();
    let holds = ChecksHolds::new(Default::default(), follow);
    let handles = [(
        PROJECT.to_string(),
        StateHandle::Writer(fixture.writer().clone()),
    )];
    holds.refresh_from(&handles);
    holds.refresh_from(&handles);
    let mut events = followed.try_recv().unwrap();
    assert!(followed.try_recv().is_err(), "followed once");
    fixture
        .writer()
        .transact(|tx| {
            tx.kv(LIVE_NAMESPACE).put(
                &hold_key("plain"),
                toml::Value::try_from(HoldRow {
                    reason: "checking the tree".to_string(),
                })?,
            )
        })
        .unwrap();
    let Some(StateEvent::Changed { namespaces, .. }) = futures::executor::block_on(events.next())
    else {
        panic!("no event");
    };
    assert_eq!(namespaces, [LIVE_NAMESPACE]);
}

const CLOSING: &str = "[[checks.closing]]\nname = \"full\"\ncommand = [\"make\", \"full\"]\n";

/// A tree with the group `grp` (its checks table `group_checks`) holding the leaves `children`,
/// each `(id, checks)`, in that order (the slugs sort as the ids), plus `extra` tasks.
fn group_tree(
    group_checks: &str,
    children: &[(&str, &str)],
    extra: &[(&str, String)],
) -> DefinitionTree {
    let mut tasks = vec![(
        "tasks/grp".to_string(),
        task_toml("grp", "other", &[], group_checks),
    )];
    for (id, checks) in children {
        tasks.push((
            format!("tasks/grp/tasks/{id}"),
            task_toml(id, "other", &["a", "b"], checks),
        ));
    }
    for (folder, text) in extra {
        tasks.push((folder.to_string(), text.clone()));
    }
    let tasks: Vec<(&str, String)> = tasks
        .iter()
        .map(|(folder, text)| (folder.as_str(), text.clone()))
        .collect();
    tree_of(&tasks)
}

impl Fixture {
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

    fn seq(&self) -> Option<i64> {
        self.writer()
            .read(|read| {
                Ok(read
                    .kv(SEQ_NAMESPACE)
                    .get(SEQ_KEY)?
                    .and_then(|value| value.as_integer()))
            })
            .unwrap()
    }

    /// Finishes every task of `ids` in one transaction.
    fn finish_together(&self, ids: &[&str]) {
        self.writer()
            .transact(|tx| {
                for id in ids {
                    tx.set_task(id, closed())?;
                }
                Ok(())
            })
            .unwrap();
    }
}

fn carries(closing_from: &[&str]) -> OwedMarker {
    OwedMarker {
        closing_from: closing_from.iter().map(|id| id.to_string()).collect(),
        last_child: None,
    }
}

fn last_row(id: &str, generation: u64) -> LastRow {
    LastRow {
        id: id.to_string(),
        generation,
        tree_key: None,
    }
}

fn group_marker(id: &str, generation: u64) -> OwedMarker {
    OwedMarker {
        closing_from: Vec::new(),
        last_child: Some(LastChild {
            id: id.to_string(),
            generation,
        }),
    }
}

#[test]
fn the_last_child_to_finish_while_every_sibling_is_done_carries_the_closing_checks() {
    // c1 owes tests and has passed them, c2 owes nothing, c3 owes tests.
    let tree = group_tree(CLOSING, &[("c1", TESTS), ("c2", ""), ("c3", TESTS)], &[]);
    let fixture = Fixture::with(true, tree);
    for id in ["c1", "c2", "c3"] {
        fixture.set(id, last_open()).unwrap();
    }

    fixture.set("c1", closed()).unwrap();
    assert_eq!(
        fixture.marker("c1"),
        Some(carries(&[])),
        "c1 is not the last"
    );
    assert_eq!(fixture.last("grp"), Some(last_row("c1", 1)));
    fixture.seed("c1", (true, true));

    fixture.set("c2", closed()).unwrap();
    assert_eq!(
        fixture.marker("c2"),
        None,
        "c2 owes nothing and is not the last"
    );
    assert_eq!(fixture.last("grp"), Some(last_row("c2", 2)));
    assert_eq!(fixture.marker("grp"), None, "the group is still open");

    fixture.set("c3", closed()).unwrap();
    assert_eq!(fixture.marker("c3"), Some(carries(&["grp"])));
    assert_eq!(
        fixture.marker("c1"),
        Some(carries(&[])),
        "no earlier child carries"
    );
    assert_eq!(fixture.marker("c2"), None);
    assert_eq!(fixture.last("grp"), Some(last_row("c3", 3)));
    assert_eq!(fixture.marker("grp"), Some(group_marker("c3", 3)));
    assert_eq!(fixture.seq(), Some(3));
    assert!(fixture.holds().contains_key("grp"));

    // Reopened, the group loses its marker; the last child row stays.
    fixture.set("c3", last_open()).unwrap();
    assert_eq!(fixture.rows("grp"), (false, false));
    assert_eq!(fixture.rows("c3"), (false, false));
    assert_eq!(fixture.last("grp"), Some(last_row("c3", 3)));
}

#[test]
fn a_last_child_owing_nothing_itself_carries_the_closing_checks() {
    let tree = group_tree(CLOSING, &[("c1", ""), ("c2", "")], &[]);
    let fixture = Fixture::with(true, tree);
    fixture.set("c1", closed()).unwrap();
    fixture.set("c2", closed()).unwrap();
    assert_eq!(fixture.marker("c1"), None);
    assert_eq!(fixture.marker("c2"), Some(carries(&["grp"])));
    assert_eq!(fixture.marker("grp"), Some(group_marker("c2", 2)));
}

#[test]
fn a_held_sibling_or_one_finishing_in_the_same_transaction_means_no_child_carries() {
    // c1 finished but has not passed its checks: c2 does not carry them.
    let tree = group_tree(CLOSING, &[("c1", TESTS), ("c2", TESTS)], &[]);
    let fixture = Fixture::with(true, tree);
    fixture.set("c1", closed()).unwrap();
    fixture.set("c2", closed()).unwrap();
    assert_eq!(fixture.marker("c2"), Some(carries(&[])));
    assert_eq!(fixture.marker("grp"), Some(group_marker("c2", 2)));

    // Children finishing together: none carries, the last in child order is recorded, and the
    // group is marked to judge its closing checks itself.
    let tree = group_tree(CLOSING, &[("c1", ""), ("c2", TESTS), ("c3", "")], &[]);
    let fixture = Fixture::with(true, tree);
    fixture.set("c1", closed()).unwrap();
    fixture.finish_together(&["c3", "c2"]);
    assert_eq!(fixture.marker("c2"), Some(carries(&[])));
    assert_eq!(fixture.marker("c3"), None);
    assert_eq!(fixture.last("grp"), Some(last_row("c3", 2)));
    assert_eq!(fixture.marker("grp"), Some(group_marker("c3", 2)));
}

#[test]
fn closing_checks_of_a_group_above_pass_through_a_group_that_is_its_last_open_child() {
    // top: outer (closing) > inner (no closing of its own) > leaf.
    let tree = tree_of(&[
        ("tasks/outer", task_toml("outer", "other", &[], CLOSING)),
        (
            "tasks/outer/tasks/inner",
            task_toml("inner", "other", &[], ""),
        ),
        (
            "tasks/outer/tasks/inner/tasks/leaf",
            task_toml("leaf", "other", &["a", "b"], ""),
        ),
    ]);
    let fixture = Fixture::with(true, tree);
    fixture.set("leaf", closed()).unwrap();
    assert_eq!(fixture.marker("leaf"), Some(carries(&["outer"])));
    // inner carries outer's closing checks as its closing set, so it is marked too.
    assert_eq!(
        fixture.marker("inner"),
        Some(OwedMarker {
            closing_from: vec!["outer".to_string()],
            last_child: Some(LastChild {
                id: "leaf".to_string(),
                generation: 1,
            }),
        })
    );
    assert_eq!(fixture.last("inner"), Some(last_row("leaf", 1)));
    assert_eq!(fixture.last("outer"), Some(last_row("inner", 2)));
    assert_eq!(fixture.marker("outer"), Some(group_marker("inner", 2)));
    // The top group has no closing set: no row, no marker.
    assert_eq!(fixture.last("g1"), None);
    assert_eq!(fixture.marker("g1"), None);
}

#[test]
fn a_group_with_inherited_checks_and_no_closing_checks_gets_no_marker_and_is_done() {
    let tree = group_tree(TESTS, &[("c1", ""), ("c2", "")], &[]);
    let fixture = Fixture::with(true, tree);
    fixture.set("c1", closed()).unwrap();
    assert_eq!(
        fixture.marker("c1"),
        Some(carries(&[])),
        "c1 inherits tests"
    );
    fixture.seed("c1", (true, true));
    fixture.set("c2", closed()).unwrap();
    assert_eq!(fixture.marker("c2"), Some(carries(&[])));
    fixture.seed("c2", (true, true));
    assert_eq!(fixture.rows("grp"), (false, false));
    assert_eq!(
        fixture.last("grp"),
        None,
        "no closing set, no last child row"
    );
    assert_eq!(fixture.seq(), None);
    assert!(fixture.holds().is_empty(), "{:?}", fixture.holds());
}

#[test]
fn a_child_owing_nothing_or_finishing_while_off_is_still_recorded() {
    let tree = group_tree(CLOSING, &[("c1", ""), ("c2", TESTS), ("c3", TESTS)], &[]);
    let fixture = Fixture::with(true, tree);
    fixture.set("c1", closed()).unwrap();
    assert_eq!(fixture.marker("c1"), None);
    assert_eq!(fixture.last("grp"), Some(last_row("c1", 1)));

    fixture.hook.set_enabled(false);
    fixture.set("c2", closed()).unwrap();
    assert_eq!(fixture.marker("c2"), None);
    assert_eq!(fixture.last("grp"), Some(last_row("c2", 2)));
    fixture.set("c3", closed()).unwrap();
    assert_eq!(fixture.rows("c3"), (false, false));
    assert_eq!(
        fixture.rows("grp"),
        (false, false),
        "off: the group is not marked"
    );
    assert_eq!(fixture.last("grp"), Some(last_row("c3", 3)));
}

#[test]
fn a_child_moved_out_of_the_group_is_ignored() {
    let before = group_tree(CLOSING, &[("c1", TESTS), ("c2", TESTS), ("c3", TESTS)], &[]);
    let mut fixture = Fixture::with(true, before);
    for id in ["c1", "c2", "c3"] {
        fixture.set(id, last_open()).unwrap();
    }
    fixture.set("c1", closed()).unwrap();
    // c1 passed its checks: its done record carries generation 1.
    fixture.seed("c1", (true, true));
    fixture.set("c2", closed()).unwrap();
    assert_eq!(fixture.last("grp"), Some(last_row("c2", 2)));

    // c2 and the still open c3 move out of the group: only the finished c1 is left, so the
    // group finishes in the landing. Its recorded last child is gone, so c1 is used.
    fixture.land(group_tree(
        CLOSING,
        &[("c1", TESTS)],
        &[
            ("tasks/c2", task_toml("c2", "other", &["a", "b"], TESTS)),
            ("tasks/c3", task_toml("c3", "other", &["a", "b"], TESTS)),
        ],
    ));
    assert_eq!(fixture.marker("grp"), Some(group_marker("c1", 1)));

    // c3 finishing outside the group leaves its last child row alone.
    fixture.set("c3", closed()).unwrap();
    assert_eq!(fixture.marker("c3"), Some(carries(&[])));
    assert_eq!(fixture.last("grp"), Some(last_row("c2", 2)));
    assert_eq!(fixture.seq(), Some(2));
}

#[test]
fn a_group_with_no_last_child_row_gets_no_last_child() {
    // c1 finished, its row is gone (as for a group whose children all finished before it had a
    // row), and the cancelled c2 finishes the group with no child finishing: no row to copy.
    let tree = group_tree(CLOSING, &[("c1", ""), ("c2", "")], &[]);
    let fixture = Fixture::with(true, tree);
    fixture
        .writer()
        .transact(|tx| tx.set_task("c1", closed()))
        .unwrap();
    fixture
        .writer()
        .transact(|tx| tx.kv(LAST_NAMESPACE).delete("grp"))
        .unwrap();
    fixture.set("c2", cancelled()).unwrap();
    assert_eq!(fixture.marker("grp"), Some(OwedMarker::default()));
}

#[test]
fn a_snapshot_round_trip_keeps_the_counter_order_and_the_last_child() {
    use crate::exporter::ChecksExporter;
    use bongos_project_repo::StateExporter as _;

    let tree = group_tree(CLOSING, &[("c1", ""), ("c2", ""), ("c3", "")], &[]);
    let source = Fixture::with(true, tree.clone());
    source.set("c1", closed()).unwrap();
    source.set("c2", closed()).unwrap();
    assert_eq!(source.seq(), Some(2));
    let files = source
        .writer()
        .read(|read| ChecksExporter.export(read))
        .unwrap();

    let target = Fixture::with(true, tree);
    target
        .writer()
        .transact(|tx| ChecksExporter.import(tx, &files))
        .unwrap();
    assert_eq!(target.seq(), Some(2));
    assert_eq!(target.last("grp"), Some(last_row("c2", 2)));
    // The next event orders after every one before the snapshot.
    target.set("c1", closed()).unwrap();
    target.set("c2", closed()).unwrap();
    target.set("c3", closed()).unwrap();
    assert_eq!(target.last("grp"), Some(last_row("c3", 5)));
}

mod registered_in_init {
    //! The hook on the global registry, registered by `init` before any project opens. Runs
    //! alone in its process, as every test does under nextest.

    use bongos_work::DoneHolds;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use settings::SettingsStore;

    use super::*;

    fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.task_checks.get_or_insert_default().enabled = Some(enabled);
                });
            });
        });
    }

    #[gpui::test]
    fn init_registers_the_hook_and_the_source_follows_the_setting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            crate::init(cx);
        });
        let hook = crate::owed::global_hook().expect("init registers the hook");
        assert!(!hook.is_enabled());
        cx.update(|cx| assert!(!DoneHolds::has_sources(cx)));

        set_enabled(cx, true);
        assert!(hook.is_enabled());
        cx.update(|cx| assert!(DoneHolds::has_sources(cx)));

        // A project opened after init runs the hook.
        let root = TempDir::new().unwrap();
        let writer = StateStore::open_writer(&location(&root)).unwrap();
        writer.set_definitions(Arc::new(leaves()));
        writer
            .transact(|tx| tx.set_task("plain", last_open()))
            .unwrap();
        writer
            .transact(|tx| tx.set_task("plain", closed()))
            .unwrap();
        assert!(
            writer
                .read(|read| read.kv(OWED_NAMESPACE).get("plain"))
                .unwrap()
                .is_some()
        );

        set_enabled(cx, false);
        assert!(!hook.is_enabled());
        cx.update(|cx| assert!(!DoneHolds::has_sources(cx)));
        // Off: the delete half still runs.
        writer
            .transact(|tx| tx.set_task("plain", last_open()))
            .unwrap();
        assert!(
            writer
                .read(|read| read.kv(OWED_NAMESPACE).get("plain"))
                .unwrap()
                .is_none()
        );
    }
}
