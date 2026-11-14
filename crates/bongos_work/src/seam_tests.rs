//! The seams of new-format projects: project sources, definition writers, the write outcome,
//! the attestation hand-over and the old-format lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bongos_project_repo::{
    Git, Identity, LoadedProject, Location, MAIN_BRANCH, ProjectRepo, ProjectRepositorySettings,
    StateApplyHooks, StateHandle, StateStore,
};
use bongos_work_model::commands::{self, Change, Context};
use bongos_work_model::layout;
use bongos_work_model::store::{Project, Task as WorkTask, WorkTree};
use bongos_work_model::{Kind, PersonAtKeyboard, Status, TaskFile};
use fs::RealFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use parking_lot::Mutex;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::store::NO_DEFINITION_WRITER;
use crate::store_tests::{canonical, project_toml, set_enabled, task_toml, write};
use crate::{
    DefinitionWriter, DefinitionWriters, GlobalWorkStore, ProjectSource, ProjectSources, WorkDirs,
    WorkSettings, WorkStore, WriteError, WriteOutcome,
};

fn new_store(cx: &mut TestAppContext, root: &Path) -> Entity<WorkStore> {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        WorkSettings::register(cx);
        ProjectRepositorySettings::register(cx);
    });
    set_enabled(cx, true);
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let dirs = WorkDirs::for_test(root.to_path_buf());
    let store = cx.new(|cx| WorkStore::new(fs, dirs, cx));
    cx.update(|cx| cx.set_global(GlobalWorkStore(store.clone())));
    store
}

async fn wait_until(
    cx: &mut TestAppContext,
    store: &Entity<WorkStore>,
    what: &str,
    check: impl Fn(&WorkTree) -> bool,
) {
    for _ in 0..200 {
        if store.read_with(cx, |store, _| {
            store.snapshot().is_some_and(|tree| check(&tree))
        }) {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// An old-format project `invoicing` with the leaf `l1`.
fn seed(root: &Path) -> PathBuf {
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(&layout::task_file(&project), &task_toml("g1", "Invoicing"));
    let backfill = layout::child_dir(&project, "backfill");
    write(&layout::task_file(&backfill), &task_toml("l1", "Backfill"));
    backfill
}

fn close(id: &'static str) -> impl Fn(&WorkTree, &Context) -> commands::Outcome + Send + 'static {
    move |tree, ctx| commands::close(tree, ctx, id)
}

fn ctx() -> Context {
    Context::new("tester", "2026-11-13T10:00:00Z".parse().unwrap())
}

/// A new-format project held in memory: its tasks' directories are under `files`.
fn memory_project(files: &Path) -> Project {
    let mut file = TaskFile::new("Memory", Kind::Other);
    file.id = "m1".to_string();
    let mut leaf = TaskFile::new("Remembered", Kind::Other);
    leaf.id = "m2".to_string();
    let task = |slug: &str, file: TaskFile, definition: &str| WorkTask {
        slug: slug.to_string(),
        dir: files.join(&file.id),
        definition: Some(PathBuf::from(definition)),
        pending: None,
        needs_conversion: false,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: Default::default(),
        hashes: Default::default(),
        children: Vec::new(),
    };
    let mut top = task("memory", file, "");
    top.children
        .push(task("remembered", leaf, "tasks/remembered"));
    Project {
        slug: "memory".to_string(),
        file: None,
        task: top,
    }
}

struct FakeSource {
    projects: Mutex<Vec<Project>>,
}

impl ProjectSource for FakeSource {
    fn name(&self) -> &str {
        "fake"
    }

    fn projects(&self) -> Vec<Project> {
        self.projects.lock().clone()
    }
}

/// Owns changes to new-format tasks; records what it got.
struct FakeWriter {
    landing: bool,
    calls: AtomicUsize,
    attested: Mutex<Vec<bool>>,
}

impl DefinitionWriter for FakeWriter {
    fn name(&self) -> &str {
        "fake"
    }

    fn owns(&self, tree: &WorkTree, change: &Change) -> bool {
        tree.task_by_dir(&change.dir)
            .is_some_and(|task| task.definition.is_some())
    }

    fn write(
        &self,
        _tree: &WorkTree,
        _ctx: &Context,
        attestation: Option<PersonAtKeyboard>,
        change: Change,
    ) -> Result<WriteOutcome, WriteError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.attested.lock().push(attestation.is_some());
        if self.landing {
            Ok(WriteOutcome::Landing {
                change,
                change_id: "c1".to_string(),
            })
        } else {
            Ok(WriteOutcome::Applied(change))
        }
    }
}

#[gpui::test]
async fn seams_do_nothing_while_unregistered(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let backfill = seed(&root);
    let store = new_store(cx, &root);
    wait_until(cx, &store, "the first load", |_| true).await;

    let outcome = store
        .update(cx, |store, cx| store.write_outcome(ctx(), close("l1"), cx))
        .await
        .unwrap();
    assert!(matches!(outcome, WriteOutcome::Applied(_)));
    let text = std::fs::read_to_string(layout::task_file(&backfill)).unwrap();
    assert!(text.contains("status = \"done\""));
    let tree = store.read_with(cx, |store, _| store.snapshot().unwrap());
    assert_eq!(tree.projects.len(), 1);
    assert!(tree.tasks().iter().all(|task| !task.needs_conversion));
}

#[gpui::test]
async fn a_source_is_merged_and_an_owning_writer_gets_the_change(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let files = canonical(&temp).join("files");
    let store = new_store(cx, &root);
    wait_until(cx, &store, "the first load", |_| true).await;

    let source = Arc::new(FakeSource {
        projects: Mutex::new(vec![memory_project(&files)]),
    });
    cx.update(|cx| ProjectSources::register(source.clone(), cx));
    wait_until(cx, &store, "the source's project", |tree| {
        tree.task_by_id("m2").is_some()
    })
    .await;

    let writer = Arc::new(FakeWriter {
        landing: true,
        calls: AtomicUsize::new(0),
        attested: Mutex::new(Vec::new()),
    });
    cx.update(|cx| DefinitionWriters::register(writer.clone(), cx));

    // The command sees no attestation; the writer gets it.
    let seen_attestation = Arc::new(Mutex::new(Vec::new()));
    let context = Context::at_keyboard(PersonAtKeyboard::for_test());
    let outcome = store
        .update(cx, |store, cx| {
            let seen = seen_attestation.clone();
            store.write_outcome(
                context,
                move |tree, ctx| {
                    seen.lock().push(ctx.has_attestation());
                    commands::close(tree, ctx, "m2")
                },
                cx,
            )
        })
        .await
        .unwrap();
    let WriteOutcome::Landing { change_id, change } = outcome else {
        panic!("the fake writer lands");
    };
    assert_eq!(change_id, "c1");
    assert_eq!(change.entry.event, "closed");
    assert_eq!(*seen_attestation.lock(), [false]);
    assert_eq!(*writer.attested.lock(), [true]);
    // Nothing was written to disk for the new-format task.
    assert!(!files.exists());

    // `write` keeps its signature and answers with the change.
    let change = store
        .update(cx, |store, cx| store.write(ctx(), close("m2"), cx))
        .await
        .unwrap();
    assert_eq!(change.entry.event, "closed");
    assert_eq!(writer.calls.load(Ordering::SeqCst), 2);

    // An old-format task still goes to the disk.
    store
        .update(cx, |store, cx| store.write(ctx(), close("l1"), cx))
        .await
        .unwrap();
    assert_eq!(writer.calls.load(Ordering::SeqCst), 2);

    // A source that changed shows after a refresh.
    source.projects.lock().clear();
    store.update(cx, |store, cx| store.refresh_sources(cx));
    wait_until(cx, &store, "the source's project to go", |tree| {
        tree.task_by_id("m2").is_none()
    })
    .await;
}

#[gpui::test]
async fn a_new_format_change_no_writer_owns_is_refused(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let files = canonical(&temp).join("files");
    let store = new_store(cx, &root);
    wait_until(cx, &store, "the first load", |_| true).await;
    let source = Arc::new(FakeSource {
        projects: Mutex::new(vec![memory_project(&files)]),
    });
    cx.update(|cx| ProjectSources::register(source, cx));
    wait_until(cx, &store, "the source's project", |tree| {
        tree.task_by_id("m2").is_some()
    })
    .await;

    // No writer at all.
    let error = store
        .update(cx, |store, cx| store.write(ctx(), close("m2"), cx))
        .await
        .unwrap_err();
    assert_eq!(error, WriteError::Refused(NO_DEFINITION_WRITER.to_string()));
    assert!(!files.exists());

    // A writer that owns nothing.
    struct OwnsNothing;
    impl DefinitionWriter for OwnsNothing {
        fn name(&self) -> &str {
            "owns-nothing"
        }
        fn owns(&self, _tree: &WorkTree, _change: &Change) -> bool {
            false
        }
        fn write(
            &self,
            _tree: &WorkTree,
            _ctx: &Context,
            _attestation: Option<PersonAtKeyboard>,
            _change: Change,
        ) -> Result<WriteOutcome, WriteError> {
            panic!("owns nothing")
        }
    }
    cx.update(|cx| DefinitionWriters::register(Arc::new(OwnsNothing), cx));
    let error = store
        .update(cx, |store, cx| store.write(ctx(), close("m2"), cx))
        .await
        .unwrap_err();
    assert_eq!(error, WriteError::Refused(NO_DEFINITION_WRITER.to_string()));
    assert!(!files.exists());

    // An old-format task is still written to the disk.
    store
        .update(cx, |store, cx| store.write(ctx(), close("l1"), cx))
        .await
        .unwrap();
}

#[gpui::test]
async fn old_format_projects_refuse_writes_once_project_repositories_are_enabled(
    cx: &mut TestAppContext,
) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let store = new_store(cx, &root);
    wait_until(cx, &store, "the first load", |_| true).await;
    cx.update(|cx| {
        SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content.project_repository.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    assert!(cx.update(|cx| ProjectRepositorySettings::get_global(cx).enabled));
    wait_until(cx, &store, "the conversion lock", |tree| {
        tree.task_by_id("l1")
            .is_some_and(|task| task.needs_conversion)
    })
    .await;
    let error = store
        .update(cx, |store, cx| store.write(ctx(), close("l1"), cx))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("old format, needs conversion: run project: convert"),
        "{error}"
    );
}

/// A project repository with the top group `g1` and the leaf `l1`, its state store open.
pub(crate) fn project_repo(folder: &Path) -> ProjectRepo {
    let location = Location {
        id: "project-1".to_string(),
        folder: folder.to_path_buf(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        state: folder.join("state"),
        files: folder.join("files"),
        remote: None,
        converted_from: None,
    };
    let definitions = location.repository.join("definitions");
    let definition = |id: &str, goal: &str| {
        format!(
            "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
             created = 2026-01-01T10:00:00Z\n\n[[todos]]\nid = \"{id}-t\"\nname = \"do it\"\n"
        )
    };
    write(
        &definitions.join("project.toml"),
        "schema = 1\nid = \"project-1\"\ntitle = \"Repo project\"\n",
    );
    write(
        &definitions.join("task.toml"),
        &definition("g1", "Repo project"),
    );
    write(
        &definitions.join("tasks/leaf/task.toml"),
        &definition("r1", "Repo leaf"),
    );
    std::fs::create_dir_all(&location.state).unwrap();
    std::fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    futures::executor::block_on(async {
        git.init_repo(&location.repository, MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer =
        Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap());
    ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer))
}

#[gpui::test]
async fn a_project_repository_serves_its_tasks_and_applies_state_only_writes(
    cx: &mut TestAppContext,
) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let store = new_store(cx, &root);
    let repo = project_repo(&canonical(&temp).join("project"));
    let refs = || {
        futures::executor::block_on(
            repo.git()
                .run(&repo.location().repository, &["for-each-ref"]),
        )
        .unwrap()
    };
    let refs_before = refs();
    let loaded = LoadedProject::new(repo.clone());
    let _registration = cx.update(|cx| crate::register_project_repo(loaded, cx));
    wait_until(cx, &store, "the repository's project", |tree| {
        tree.task_by_id("r1").is_some()
    })
    .await;

    store
        .update(cx, |store, cx| {
            store.write(
                ctx(),
                |tree, ctx| commands::advance(tree, ctx, "r1", false),
                cx,
            )
        })
        .await
        .unwrap();
    let tree = store.read_with(cx, |store, _| store.snapshot().unwrap());
    let leaf = tree.task_by_id("r1").unwrap().file.clone().unwrap();
    assert_eq!(leaf.todos[0].state, bongos_work_model::TodoState::Done);
    let state = repo
        .state()
        .unwrap()
        .read(|read| read.task("r1"))
        .unwrap()
        .unwrap();
    assert_eq!(state.todos[0].state, bongos_work_model::TodoState::Done);
    store
        .update(cx, |store, cx| store.write(ctx(), close("r1"), cx))
        .await
        .unwrap();
    let tree = store.read_with(cx, |store, _| store.snapshot().unwrap());
    assert_eq!(
        tree.task_by_id("r1").unwrap().file.as_ref().unwrap().status,
        Status::Done
    );
    assert_eq!(refs(), refs_before);

    // A change to definitions is a quick edit: a branch and a landing, the task shown pending,
    // master untouched until it lands. Unattributed, it waits for project: approve.
    let outcome = store
        .update(cx, |store, cx| {
            store.write_outcome(
                ctx(),
                |tree, ctx| {
                    commands::edit(
                        tree,
                        ctx,
                        "r1",
                        commands::TaskEdit {
                            goal: Some("Renamed leaf".to_string()),
                            kind: None,
                            repository: None,
                        },
                    )
                },
                cx,
            )
        })
        .await
        .unwrap();
    let WriteOutcome::Landing { change_id, .. } = outcome else {
        panic!("expected a landing, got {outcome:?}");
    };
    let tree = store.read_with(cx, |store, _| store.snapshot().unwrap());
    let leaf = tree.task_by_id("r1").unwrap();
    assert_eq!(leaf.goal(), "Repo leaf");
    let pending = leaf.pending.clone().expect("the leaf is pending");
    assert_eq!(pending.change_id, change_id);
    assert_eq!(pending.label, bongos_project_repo::LANDING_MARK);
    let main = || {
        futures::executor::block_on(
            repo.git()
                .rev_parse(&repo.location().repository, "refs/heads/main"),
        )
        .unwrap()
    };
    assert!(refs_before.contains(&main()));
    let row = repo.change(&change_id).unwrap().unwrap();
    assert!(refs().contains(&format!("refs/heads/{}", row.branch)));
}
