use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use bongos_project_repo_model::TaskStateRecord;
use bongos_work_model::store::{self, Task, WorkTree};
use bongos_work_model::{
    Edge, Extra, HistoryEntry, HistoryFile, Kind, ProjectFile, ResultKind, Status, TaskFile,
    TaskResult, Timestamp, Todo, TodoState, UnblockPoint, serialize_history, serialize_project,
    serialize_task,
};
use futures::executor::block_on;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

use crate::convert::{Phase, Seams, TempLock, claim_conversion, convert_with};
use crate::test_support::git;
use crate::{
    CONVERSION_COMMIT_MESSAGE, ConversionPreview, ConvertError, ConvertOptions, LayoutTemplates,
    OldFormatState, OldSource, ProjectRepo, RepositoryLocations, StateApplyChange, StateApplyHook,
    StateApplyHooks, StateHandle, StateStore, StateTx, TreeRef, claim_old_project_start,
    clean_stale_conversions, convert, convert_all, preview, preview_all,
};

const TITLE: &str = "One-offs";

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// Counts every time it runs.
struct CountingHook {
    calls: AtomicUsize,
}

impl StateApplyHook for CountingHook {
    fn name(&self) -> &str {
        "counter"
    }

    fn wants(&self, _change: &StateApplyChange) -> bool {
        true
    }

    fn apply(&self, _tx: &mut StateTx, _change: &StateApplyChange) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A machine: the old work root, the projects folder, the location record, and the hooks.
struct World {
    dir: TempDir,
    hook: Arc<CountingHook>,
    hooks: StateApplyHooks,
    layout: LayoutTemplates,
    projects_dir: PathBuf,
}

impl World {
    fn new() -> Self {
        let hook = Arc::new(CountingHook {
            calls: AtomicUsize::new(0),
        });
        let hooks = StateApplyHooks::new();
        hooks.register(hook.clone());
        let dir = TempDir::new().unwrap();
        let projects_dir = dir.path().join("projects");
        let world = Self {
            dir,
            hook,
            hooks,
            layout: LayoutTemplates::default(),
            projects_dir,
        };
        std::fs::create_dir_all(world.root()).unwrap();
        world
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn projects_dir(&self) -> PathBuf {
        self.projects_dir.clone()
    }

    fn old_project(&self) -> PathBuf {
        self.root().join("projects/invoicing")
    }

    fn options(&self) -> ConvertOptions<'_> {
        ConvertOptions {
            projects_dir: &self.projects_dir,
            layout: &self.layout,
            hooks: &self.hooks,
        }
    }

    fn locations(&self) -> RepositoryLocations {
        RepositoryLocations::load(self.dir.path().join("locations.toml")).unwrap()
    }

    fn load(&self) -> WorkTree {
        store::load(&self.root())
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.root().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn hook_calls(&self) -> usize {
        self.hook.calls.load(Ordering::SeqCst)
    }
}

fn ids() -> impl FnMut() -> String {
    let mut next = 0;
    move || {
        next += 1;
        format!("0190-new-{next}")
    }
}

fn task_file(id: &str, goal: &str, status: Status) -> TaskFile {
    let mut file = TaskFile::new(goal, Kind::Other);
    file.id = id.to_string();
    file.created = at("2026-01-01T10:00:00Z");
    file.status = status;
    file
}

fn history(event: &str) -> HistoryEntry {
    HistoryEntry {
        at: at("2026-01-02T10:00:00Z"),
        by: "kyle".to_string(),
        event: event.to_string(),
        detail: None,
        extra: Extra::new(),
    }
}

/// The old project `invoicing`: the top group `g1`, `backfill` (`l1`: done, two todos, a
/// result, `after` report, a chat, a run, a proposal, an artifact and an old history) and
/// `report` (`l2`, open), a workflow and a plan.
fn write_fixture(world: &World) {
    let mut project = ProjectFile::new("Invoicing");
    project.id = Some("p-invoicing".to_string());
    world.write(
        "projects/invoicing/project.toml",
        &serialize_project(&project).unwrap(),
    );
    world.write(
        "projects/invoicing/task.toml",
        &serialize_task(&task_file("g1", "Invoicing", Status::Open)).unwrap(),
    );
    world.write("projects/invoicing/plan.md", "# The plan\n");
    world.write("projects/invoicing/prompts.txt", "project prompts\n");
    world.write(
        "projects/invoicing/workflows/ship/workflow.toml",
        "name = \"ship\"\n",
    );

    let mut backfill = task_file("l1", "Backfill", Status::Done);
    backfill.todos = ["implement", "review"]
        .into_iter()
        .map(|name| Todo {
            id: None,
            name: name.to_string(),
            role: None,
            workflow: None,
            state: TodoState::Done,
            started: None,
            extra: Extra::new(),
        })
        .collect();
    backfill.todos[1].state = TodoState::Skipped;
    backfill.after = vec![Edge {
        task: "report".to_string(),
        unblock: UnblockPoint::Merged,
        extra: Extra::new(),
    }];
    backfill.result = Some(TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://example.com/pr/1".to_string()),
        state: None,
        urls: Vec::new(),
        extra: Extra::new(),
    });
    backfill.history = vec![history("closed")];
    let task = "projects/invoicing/tasks/backfill";
    world.write(
        &format!("{task}/task.toml"),
        &serialize_task(&backfill).unwrap(),
    );
    world.write(
        &format!("{task}/history.old.toml"),
        &serialize_history(&HistoryFile {
            history: vec![history("created")],
            ..HistoryFile::default()
        })
        .unwrap(),
    );
    world.write(&format!("{task}/prompts.txt"), "task prompts\n");
    world.write(
        &format!("{task}/artifacts.toml"),
        "schema = 1\n\n[[artifact]]\npath = \"notes/design.md\"\ndescription = \"Design\"\n",
    );
    world.write(&format!("{task}/notes/design.md"), "the design\n");
    world.write(&format!("{task}/chats/c1.toml"), "schema = 1\n");
    world.write(&format!("{task}/runs/r1/log.txt"), "ran\n");
    world.write(&format!("{task}/proposals/1.proposal.txt"), "proposal\n");
    world.write(
        "projects/invoicing/tasks/report/task.toml",
        &serialize_task(&task_file("l2", "Report", Status::Open)).unwrap(),
    );
}

/// Every entry under `dir`: its kind and, for a file, the hash of its bytes; for a link, its
/// target. Never follows a link.
fn hash_tree(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(base).unwrap().to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(relative, format!("link {}", target.display()));
            } else if metadata.is_dir() {
                out.insert(relative, "dir".to_string());
                visit(&path, base, out);
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(relative, format!("file {:x}", Sha256::digest(&bytes)));
            } else {
                out.insert(relative, "other".to_string());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(dir, dir, &mut out);
    out
}

fn the_project(tree: &WorkTree) -> OldSource<'_> {
    OldSource::Project(&tree.projects[0])
}

fn converted_tree(location: &crate::Location) -> WorkTree {
    let reader = StateStore::open_reader(location).unwrap();
    let repo =
        ProjectRepo::new(location.clone(), git()).with_state(StateHandle::Reader(Arc::new(reader)));
    block_on(async {
        let commit = repo.shown_commit().await.unwrap();
        repo.load_work_tree_at(&TreeRef::Commit(commit))
            .await
            .unwrap()
    })
}

/// The task file with its todo ids cleared, to compare an old task with its conversion.
fn without_todo_ids(task: &Task) -> TaskFile {
    let mut file = task.file.clone().unwrap();
    for todo in &mut file.todos {
        todo.id = None;
    }
    file
}

#[test]
fn a_conversion_keeps_every_task_and_runs_no_state_hook() {
    let world = World::new();
    write_fixture(&world);
    let tree = world.load();
    let before = hash_tree(&world.root());
    let mut locations = world.locations();

    let converted = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut locations,
        &git(),
        ids(),
    ))
    .unwrap();

    assert_eq!(hash_tree(&world.root()), before, "the old folder changed");
    assert_eq!(world.hook_calls(), 0, "a state apply hook ran");
    let location = &converted.location;
    assert_eq!(location.folder, world.projects_dir().join("invoicing"));
    assert_eq!(location.id, "p-invoicing");
    assert_eq!(
        location.converted_from.as_deref(),
        Some(world.old_project().as_path())
    );
    let reloaded = world.locations();
    assert_eq!(reloaded.locations(), std::slice::from_ref(location));
    assert_eq!(
        reloaded.old_format_state(&world.old_project()),
        OldFormatState::Converted {
            folder: location.folder.clone()
        }
    );

    // One commit, with the conversion's message.
    let log = block_on(git().run(&location.repository, &["log", "--format=%s", "main"])).unwrap();
    assert_eq!(log, CONVERSION_COMMIT_MESSAGE);

    let new = converted_tree(location);
    let new_tasks = new.tasks();
    assert_eq!(new_tasks.len(), 3);
    for old in tree.tasks() {
        let id = old.id().unwrap();
        let task = new.task_by_id(id).unwrap();
        let mut expected = without_todo_ids(old);
        if id == "l1" {
            expected.history.insert(0, history("created"));
        }
        assert_eq!(without_todo_ids(task), expected, "task {id}");
        assert!(
            task.file
                .as_ref()
                .unwrap()
                .todos
                .iter()
                .all(|todo| todo.id.is_some())
        );
    }
    let backfill = new.task_by_id("l1").unwrap();
    assert_eq!(backfill.slug, "backfill");
    assert!(backfill.broken_edges.is_empty());
    // Chats open from their task's folder; artifacts are reachable.
    assert_eq!(
        std::fs::read_to_string(backfill.dir.join("chats/c1.toml")).unwrap(),
        "schema = 1\n"
    );
    let artifacts = backfill.artifacts.as_ref().unwrap();
    assert_eq!(artifacts.artifacts[0].path, "notes/design.md");
    assert_eq!(
        std::fs::read_to_string(backfill.dir.join("notes/design.md")).unwrap(),
        "the design\n"
    );
    for name in ["prompts.txt", "runs/r1/log.txt", "proposals/1.proposal.txt"] {
        assert!(backfill.dir.join(name).is_file(), "{name}");
    }
    assert!(!backfill.dir.join("history.old.toml").exists());
    assert!(!backfill.dir.join("task.toml").exists());
    let definitions = location.repository.join("definitions");
    assert_eq!(
        std::fs::read_to_string(definitions.join("plan.md")).unwrap(),
        "# The plan\n"
    );
    assert!(definitions.join("workflows/ship/workflow.toml").is_file());
    assert!(definitions.join("tasks/backfill/task.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(new.task_by_id("g1").unwrap().dir.join("prompts.txt")).unwrap(),
        "project prompts\n"
    );
    assert!(no_temporary_folders(&world));

    // The hooks were live all along: a normal write runs them.
    let writer = StateStore::open_writer_with_hooks(location, &world.hooks).unwrap();
    writer
        .transact(|tx| tx.set_task("l2", TaskStateRecord::default()))
        .unwrap();
    assert_eq!(world.hook_calls(), 1);
}

fn no_temporary_folders(world: &World) -> bool {
    std::fs::read_dir(world.projects_dir())
        .map(|entries| {
            entries.flatten().all(|entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".converting-")
            })
        })
        .unwrap_or(true)
}

#[test]
fn a_failure_at_any_phase_leaves_nothing_and_the_old_folder_untouched() {
    for phase in [
        Phase::Build,
        Phase::Commit,
        Phase::Import,
        Phase::Rename,
        Phase::LocationRecord,
    ] {
        let world = World::new();
        write_fixture(&world);
        let tree = world.load();
        let before = hash_tree(&world.root());
        let mut locations = world.locations();
        let result = block_on(convert_with(
            &the_project(&tree),
            world.options(),
            &mut locations,
            &git(),
            ids(),
            Seams {
                fail_at: Some(phase),
            },
        ));
        assert!(result.is_err(), "{phase:?}");
        assert_eq!(hash_tree(&world.root()), before, "{phase:?}");
        assert!(
            !world.projects_dir().exists(),
            "{phase:?} left the projects folder"
        );
        assert!(locations.locations().is_empty(), "{phase:?}");
        assert!(world.locations().locations().is_empty(), "{phase:?}");
        assert!(
            !world.dir.path().join("locations.toml").exists(),
            "{phase:?}"
        );
        assert_eq!(world.hook_calls(), 0, "{phase:?}");

        // Nothing stands in the way of converting again.
        let converted = block_on(convert(
            &the_project(&tree),
            world.options(),
            &mut locations,
            &git(),
            ids(),
        ));
        assert!(converted.is_ok(), "{phase:?}: {converted:?}");
    }
}

#[test]
fn a_failure_keeps_a_projects_folder_that_was_already_there() {
    let world = World::new();
    write_fixture(&world);
    std::fs::create_dir_all(world.projects_dir().join("other")).unwrap();
    let tree = world.load();
    let result = block_on(convert_with(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
        Seams {
            fail_at: Some(Phase::Rename),
        },
    ));
    assert!(result.is_err());
    let names: Vec<String> = std::fs::read_dir(world.projects_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["other"]);
}

#[cfg(unix)]
#[test]
fn hostile_entries_are_named_in_the_preview_and_never_copied() {
    use std::os::unix::ffi::OsStrExt as _;

    let world = World::new();
    write_fixture(&world);
    let outside = world.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
    let task = world.old_project().join("tasks/backfill");
    std::os::unix::fs::symlink(outside.join("secret.txt"), task.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, task.join("linked-dir")).unwrap();
    std::os::unix::fs::symlink(&outside, world.old_project().join("tasks/sneaky")).unwrap();
    std::fs::create_dir_all(task.join(".git")).unwrap();
    std::fs::write(task.join(".git/config"), "[core]\n").unwrap();
    std::fs::create_dir_all(task.join("notes/.git")).unwrap();
    std::fs::write(task.join("notes/.git/HEAD"), "ref\n").unwrap();
    std::fs::write(task.join("line\nbreak.txt"), "x").unwrap();
    std::fs::write(task.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")), "x").unwrap();
    std::fs::write(task.join(".x.tmp-1"), "half").unwrap();
    let tree = world.load();
    let before = hash_tree(&world.root());
    let outside_before = hash_tree(&outside);
    let locations = world.locations();

    let preview = preview(&the_project(&tree), world.options(), &locations);
    assert!(!preview.is_blocked(), "{:?}", preview.summary.blockers);
    let rejected: Vec<String> = preview
        .summary
        .rejected
        .iter()
        .map(|entry| {
            entry
                .path
                .strip_prefix(world.old_project())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for expected in [
        "tasks/backfill/link.txt",
        "tasks/backfill/linked-dir",
        "tasks/sneaky",
        "tasks/backfill/.git",
        "tasks/backfill/notes/.git",
        "tasks/backfill/line\nbreak.txt",
        "tasks/backfill/bad\u{fffd}.txt",
        "tasks/backfill/.x.tmp-1",
    ] {
        assert!(
            rejected.iter().any(|path| path == expected),
            "{expected:?} in {rejected:?}"
        );
    }
    // The preview crosses to the window as data.
    let wired: ConversionPreview =
        serde_json::from_value(serde_json::to_value(&preview).unwrap()).unwrap();
    assert_eq!(wired, preview);

    let mut locations = locations;
    let converted = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut locations,
        &git(),
        ids(),
    ))
    .unwrap();
    assert_eq!(hash_tree(&world.root()), before);
    assert_eq!(hash_tree(&outside), outside_before);
    let made = hash_tree(&converted.location.folder);
    for (path, kind) in &made {
        assert!(!kind.starts_with("link"), "{} is a link", path.display());
        let text = path.to_string_lossy();
        let in_files = text.starts_with("files/");
        assert!(
            !(in_files && (text.contains(".git") || text.contains('\n') || text.contains("tmp-"))),
            "{text:?} was copied"
        );
        assert!(!text.contains("secret"), "{text:?}");
    }
    let files = converted.location.files.join("l1");
    assert!(!files.join("link.txt").exists());
    assert!(!files.join("linked-dir").exists());
}

#[test]
fn a_hostile_task_id_blocks_the_conversion_and_makes_nothing() {
    let world = World::new();
    write_fixture(&world);
    world.write(
        "projects/invoicing/tasks/report/task.toml",
        &serialize_task(&task_file("../../escape", "Report", Status::Open)).unwrap(),
    );
    let tree = world.load();
    let before = hash_tree(&world.root());
    let mut locations = world.locations();
    let preview = preview(&the_project(&tree), world.options(), &locations);
    assert!(preview.is_blocked());
    assert_eq!(preview.destination, None);
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut locations,
        &git(),
        ids(),
    ));
    assert!(
        matches!(result, Err(ConvertError::Blocked(_))),
        "{result:?}"
    );
    assert_eq!(hash_tree(&world.root()), before);
    assert!(!world.projects_dir().exists());
    assert!(!world.dir.path().join("escape").exists());
}

#[test]
fn a_task_file_that_does_not_parse_blocks_the_conversion() {
    let world = World::new();
    write_fixture(&world);
    world.write("projects/invoicing/tasks/report/task.toml", "not = [toml");
    let tree = world.load();
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ));
    assert!(
        matches!(result, Err(ConvertError::Blocked(_))),
        "{result:?}"
    );
    assert!(!world.projects_dir().exists());
}

#[test]
fn an_unfinished_journal_blocks_the_conversion() {
    let world = World::new();
    write_fixture(&world);
    let tree = world.load();
    world.write("projects/invoicing/tasks/backfill/.op.toml", "half");
    let preview = preview(&the_project(&tree), world.options(), &world.locations());
    assert!(preview.is_blocked());
    assert!(
        preview.summary.blockers[0].contains("journal"),
        "{:?}",
        preview.summary.blockers
    );
}

#[test]
fn a_stale_temporary_folder_is_removed_and_a_running_one_kept() {
    let world = World::new();
    let projects = world.projects_dir();
    let stale = projects.join(".converting-stale");
    std::fs::create_dir_all(stale.join("repository/definitions")).unwrap();
    std::fs::write(stale.join("repository/definitions/task.toml"), "half").unwrap();
    let running = projects.join(".converting-running");
    std::fs::create_dir_all(&running).unwrap();
    let held = TempLock::take(&running).unwrap().unwrap();
    let outside = world.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, projects.join(".converting-link")).unwrap();
    std::fs::create_dir_all(projects.join("real-project")).unwrap();

    let mut removed = clean_stale_conversions(&projects);
    removed.sort();
    let mut expected = vec![stale];
    #[cfg(unix)]
    expected.push(projects.join(".converting-link"));
    expected.sort();
    assert_eq!(removed, expected);
    assert!(running.is_dir());
    assert!(outside.join("keep.txt").is_file());
    assert!(projects.join("real-project").is_dir());

    drop(held);
    assert_eq!(
        clean_stale_conversions(&projects),
        std::slice::from_ref(&running)
    );
    assert!(!running.exists());
}

#[test]
fn a_temporary_folder_is_never_taken_for_a_project_or_a_destination() {
    let world = World::new();
    write_fixture(&world);
    std::fs::create_dir_all(world.projects_dir().join(".converting-old/repository")).unwrap();
    let tree = world.load();
    let converted = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ))
    .unwrap();
    assert_eq!(
        converted.location.folder,
        world.projects_dir().join("invoicing")
    );
    // The old layout's loader never lists a hidden folder as a project either.
    let old_view = store::load(&world.projects_dir());
    assert!(old_view.projects.is_empty());
}

#[test]
fn a_second_conversion_or_a_start_is_refused_while_one_runs() {
    let world = World::new();
    write_fixture(&world);
    let tree = world.load();
    let old = world.old_project();

    let claim = claim_conversion(&old).unwrap();
    assert!(matches!(
        claim_old_project_start(&old),
        Err(ConvertError::Busy(_))
    ));
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ));
    assert!(matches!(result, Err(ConvertError::Busy(_))), "{result:?}");
    drop(claim);

    let started = claim_old_project_start(&old).unwrap();
    let also = claim_old_project_start(&old).unwrap();
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ));
    assert!(matches!(result, Err(ConvertError::Busy(_))), "{result:?}");
    drop(started);
    assert!(claim_conversion(&old).is_err());
    drop(also);

    // Two conversions at once: one converts, the other is refused.
    let mut first = world.locations();
    let mut second = world.locations();
    let source = the_project(&tree);
    let shared_git = git();
    let (a, b) = block_on(async {
        futures::join!(
            convert(&source, world.options(), &mut first, &shared_git, ids()),
            convert(&source, world.options(), &mut second, &shared_git, ids()),
        )
    });
    assert!(a.is_ok(), "{a:?}");
    assert!(matches!(b, Err(ConvertError::Busy(_))), "{b:?}");
    assert_eq!(world.locations().locations().len(), 1);
    assert!(no_temporary_folders(&world));

    // And once converted, it is never converted again.
    let again = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ));
    assert!(
        matches!(again, Err(ConvertError::AlreadyConverted { .. })),
        "{again:?}"
    );
}

#[test]
fn a_taken_folder_name_gives_the_next_free_one() {
    let world = World::new();
    write_fixture(&world);
    std::fs::create_dir_all(world.projects_dir().join("invoicing")).unwrap();
    let tree = world.load();
    let preview = preview(&the_project(&tree), world.options(), &world.locations());
    let expected = world.projects_dir().join("invoicing-2");
    assert_eq!(preview.destination.as_ref(), Some(&expected));
    let converted = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ))
    .unwrap();
    assert_eq!(converted.location.folder, expected);
    assert_eq!(
        std::fs::read_dir(world.projects_dir().join("invoicing"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn a_project_id_already_on_this_machine_is_refused() {
    let world = World::new();
    write_fixture(&world);
    let tree = world.load();
    let mut locations = world.locations();
    let mut taken = crate::test_support::location(&world.dir);
    taken.id = "p-invoicing".to_string();
    locations.record(taken).unwrap();
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut locations,
        &git(),
        ids(),
    ));
    assert!(
        matches!(result, Err(ConvertError::IdTaken { .. })),
        "{result:?}"
    );
    assert!(!world.projects_dir().exists());
}

#[test]
fn a_layout_with_a_part_outside_the_folder_is_refused_and_leaves_nothing() {
    let mut world = World::new();
    write_fixture(&world);
    world.layout.state = world
        .dir
        .path()
        .join("elsewhere/{folder}")
        .to_string_lossy()
        .into();
    let tree = world.load();
    let result = block_on(convert(
        &the_project(&tree),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ));
    assert!(result.is_err());
    assert!(!world.projects_dir().exists());
    assert!(!world.dir.path().join("elsewhere").exists());
}

#[test]
fn an_empty_project_converts() {
    let world = World::new();
    let mut project = ProjectFile::new("Empty");
    project.id = Some("p-empty".to_string());
    world.write(
        "projects/empty/project.toml",
        &serialize_project(&project).unwrap(),
    );
    world.write(
        "projects/empty/task.toml",
        &serialize_task(&task_file("g", "Empty", Status::Open)).unwrap(),
    );
    let tree = world.load();
    let converted = block_on(convert(
        &OldSource::Project(&tree.projects[0]),
        world.options(),
        &mut world.locations(),
        &git(),
        ids(),
    ))
    .unwrap();
    let new = converted_tree(&converted.location);
    assert_eq!(new.tasks().len(), 1);
    assert_eq!(converted.summary.files, 0);
}

#[test]
fn convert_all_converts_the_projects_and_the_oneoffs_into_one_project() {
    let world = World::new();
    write_fixture(&world);
    let oneoff = "oneoffs/writing/2026-01-02-fix";
    world.write(
        &format!("{oneoff}/task.toml"),
        &serialize_task(&task_file("o1", "Fix", Status::Done)).unwrap(),
    );
    world.write(&format!("{oneoff}/chats/c.toml"), "schema = 1\n");
    world.write(
        "oneoffs/reading/2026-01-03-book/task.toml",
        &serialize_task(&task_file("o2", "Book", Status::Open)).unwrap(),
    );
    let tree = world.load();
    let before = hash_tree(&world.root());
    let now = at("2026-01-05T10:00:00Z");
    let mut locations = world.locations();

    let previews = preview_all(&tree, TITLE, now, world.options(), &locations);
    assert_eq!(previews.len(), 2);
    assert_eq!(previews[1].summary.title, TITLE);
    assert_eq!(previews[1].summary.chats, 1);

    let results = block_on(convert_all(
        &tree,
        TITLE,
        now,
        world.options(),
        &mut locations,
        &git(),
        ids(),
    ));
    assert_eq!(results.len(), 2);
    for (old, result) in &results {
        assert!(result.is_ok(), "{}: {result:?}", old.display());
    }
    assert_eq!(hash_tree(&world.root()), before);
    let oneoffs = &results[1].1.as_ref().unwrap().location;
    assert_eq!(oneoffs.folder, world.projects_dir().join("one-offs"));
    assert_eq!(
        oneoffs.converted_from.as_deref(),
        Some(world.root().join("oneoffs").as_path())
    );
    let new = converted_tree(oneoffs);
    let top = &new.projects[0].task;
    let groups: Vec<(&str, Option<&str>)> = top
        .children
        .iter()
        .map(|group| {
            (
                group.slug.as_str(),
                group.file.as_ref().unwrap().activity.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [("reading", Some("reading")), ("writing", Some("writing"))]
    );
    let fix = new.task_by_id("o1").unwrap();
    assert_eq!(fix.file.as_ref().unwrap().status, Status::Done);
    assert_eq!(new.parent_of("o1").unwrap().slug, "writing");
    assert!(fix.dir.join("chats/c.toml").is_file());

    // Nothing is left to convert.
    assert!(preview_all(&tree, TITLE, now, world.options(), &locations).is_empty());
}
