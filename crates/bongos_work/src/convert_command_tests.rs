//! Old-format projects while project repositories are on: every write refused, converted ones
//! hidden and named, the explanation shown once, `project: convert` from the preview to the
//! hidden project, and one-offs filed in the one-offs project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_project_repo::router::{DefinitionOp, Routed, route};
use bongos_project_repo::{
    ConvertError, DEFAULT_ONEOFFS_TITLE, Git, LayoutTemplates, Location, ProjectRepositorySettings,
    RepositoryLocations, claim_old_project_start,
};
use bongos_work_model::commands::{self, Change, Context, NewTask, TaskEdit, Write};
use bongos_work_model::store::{NEEDS_CONVERSION, Project, Task, WorkTree, load};
use bongos_work_model::{Extra, HistoryEntry, Kind, ProjectFile, TaskFile, layout};
use fs::RealFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::convert_commands::{
    ConversionSetup, conversion_message, previews, render_previews, run_conversion,
};
use crate::oneoffs::{ensure_oneoffs_project, oneoffs_project};
use crate::store::hide_converted;
use crate::store_tests::{canonical, project_toml, set_enabled, task_toml, write};
use crate::view::OLD_FORMAT_MARK;
use crate::worktrees::{Layout, plan};
use crate::{
    CONVERSION_EXPLANATION, CONVERSION_NOTICE_KEY, GlobalWorkStore, WorkDirs, WorkSettings,
    WorkStore, WriteError,
};

fn ctx() -> Context {
    Context::new("tester", "2026-11-13T10:00:00Z".parse().unwrap())
}

/// The old-format project `invoicing` (top group `g1`, leaf `l1`) and the old one-off `o1`.
fn seed(root: &Path) -> PathBuf {
    let project = layout::project_dir(root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    write(&layout::task_file(&project), &task_toml("g1", "Invoicing"));
    let backfill = layout::child_dir(&project, "backfill");
    write(&layout::task_file(&backfill), &task_toml("l1", "Backfill"));
    let oneoff = layout::oneoffs_dir(root)
        .join("general")
        .join("2026-01-01-tidy");
    write(&layout::task_file(&oneoff), &task_toml("o1", "Tidy"));
    project
}

/// Every file under `root` and its bytes.
fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn new_store(cx: &mut TestAppContext, dirs: WorkDirs) -> Entity<WorkStore> {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        WorkSettings::register(cx);
        ProjectRepositorySettings::register(cx);
    });
    set_enabled(cx, true);
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let store = cx.new(|cx| WorkStore::new(fs, dirs, cx));
    cx.update(|cx| cx.set_global(GlobalWorkStore(store.clone())));
    store
}

fn enable_project_repositories(cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content.project_repository.get_or_insert_default().enabled = Some(true);
            });
        });
    });
}

async fn wait_until(
    cx: &mut TestAppContext,
    store: &Entity<WorkStore>,
    what: &str,
    check: impl Fn(&WorkStore) -> bool,
) {
    for _ in 0..200 {
        if store.read_with(cx, |store, _| check(store)) {
            return;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn locked(store: &WorkStore) -> bool {
    store.snapshot().is_some_and(|tree| {
        tree.task_by_id("l1")
            .is_some_and(|task| task.needs_conversion)
    })
}

/// A location record naming `old` as converted into `folder`.
fn record_converted(path: &Path, old: &Path, folder: &Path) {
    let mut locations = RepositoryLocations::load(path).unwrap();
    locations
        .record(Location {
            id: "converted-1".to_string(),
            folder: folder.to_path_buf(),
            repository: folder.join("repository"),
            working_copies: folder.join("working-copies"),
            state: folder.join("state"),
            files: folder.join("files"),
            remote: None,
            converted_from: Some(old.to_path_buf()),
        })
        .unwrap();
}

#[gpui::test]
async fn every_write_to_an_old_format_project_is_refused(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let store = new_store(cx, WorkDirs::for_test(root.clone()));
    enable_project_repositories(cx);
    wait_until(cx, &store, "the conversion lock", locked).await;
    let before = contents(&root);

    type Command = Box<dyn Fn(&WorkTree, &Context) -> commands::Outcome + Send>;
    let new_task = |goal: &str| NewTask {
        goal: goal.to_string(),
        ..NewTask::default()
    };
    let attempts: Vec<(&str, Command)> = vec![
        (
            "close",
            Box::new(|tree, ctx| commands::close(tree, ctx, "l1")),
        ),
        (
            "advance",
            Box::new(|tree, ctx| commands::advance(tree, ctx, "l1", false)),
        ),
        (
            "edit",
            Box::new(|tree, ctx| {
                commands::edit(
                    tree,
                    ctx,
                    "l1",
                    TaskEdit {
                        goal: Some("Renamed".to_string()),
                        ..TaskEdit::default()
                    },
                )
            }),
        ),
        (
            "new child",
            Box::new(move |tree, ctx| commands::new_child(tree, ctx, "g1", new_task("More"))),
        ),
        (
            "archive",
            Box::new(|tree, ctx| commands::project_archive(tree, ctx, "invoicing")),
        ),
        (
            "close a one-off",
            Box::new(|tree, ctx| commands::close(tree, ctx, "o1")),
        ),
        (
            "new old one-off",
            Box::new(move |tree, ctx| {
                commands::new_oneoff(tree, ctx, "general", new_task("Another"))
            }),
        ),
        (
            "make one-off",
            Box::new(|tree, ctx| commands::make_one_off(tree, ctx, "l1", "general")),
        ),
        // A command that skips the write guard is still refused by the writer.
        (
            "unguarded",
            Box::new(|tree, _| {
                let task = tree.task_by_id("l1").unwrap();
                let mut file = task.file.clone().unwrap();
                file.goal = "Sneaked".to_string();
                Ok(Change {
                    dir: task.dir.clone(),
                    writes: vec![Write::Task {
                        dir: task.dir.clone(),
                        file,
                    }],
                    entry: HistoryEntry {
                        at: ctx().at,
                        by: "tester".to_string(),
                        event: "edited".to_string(),
                        detail: None,
                        extra: Extra::new(),
                    },
                    moves: Vec::new(),
                })
            }),
        ),
    ];
    for (name, command) in attempts {
        let error = store
            .update(cx, |store, cx| {
                store.write(ctx(), move |tree, ctx| command(tree, ctx), cx)
            })
            .await
            .unwrap_err();
        let WriteError::Refused(reason) = &error else {
            panic!("{name}: {error:?}");
        };
        assert!(
            reason.contains(NEEDS_CONVERSION) || reason.contains("needs conversion"),
            "{name}: {reason}"
        );
    }
    assert_eq!(contents(&root), before, "nothing was written");

    // `task: start` refuses before making a worktree.
    let tree = store.read_with(cx, |store, _| store.snapshot().unwrap());
    let layout = Layout {
        default_root: None,
        home: canonical(&temp).join("home"),
        central: canonical(&temp).join("central"),
    };
    let refusal = plan(&tree, "l1", &[], &layout).err().unwrap();
    assert!(refusal.ends_with(NEEDS_CONVERSION), "{refusal}");

    // The rows say why.
    let notice = store
        .read_with(cx, |store, _| store.conversion_notice())
        .unwrap();
    assert_eq!(notice.needs_conversion, ["Invoicing", "one-offs"]);
    assert!(NEEDS_CONVERSION.starts_with(OLD_FORMAT_MARK));
}

#[gpui::test]
async fn a_converted_project_is_hidden_and_named_in_the_notice(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let root = base.join("work");
    let project = seed(&root);
    let record = base.join("locations.toml");
    record_converted(&record, &project, &base.join("projects").join("invoicing"));
    let dirs = WorkDirs {
        locations: Some(record),
        ..WorkDirs::for_test(root.clone())
    };
    let store = new_store(cx, dirs);
    wait_until(cx, &store, "the first load", |store| {
        store.snapshot().is_some()
    })
    .await;
    // While project repositories are off, nothing is hidden.
    assert!(store.read_with(cx, |store, _| {
        store.snapshot().unwrap().task_by_id("g1").is_some()
    }));

    enable_project_repositories(cx);
    wait_until(cx, &store, "the converted project to go", |store| {
        store
            .snapshot()
            .is_some_and(|tree| tree.task_by_id("g1").is_none())
    })
    .await;
    let notice = store
        .read_with(cx, |store, _| store.conversion_notice())
        .unwrap();
    assert_eq!(notice.converted.len(), 1);
    assert_eq!(notice.converted[0].name, "Invoicing");
    assert_eq!(notice.needs_conversion, ["one-offs"]);
    let text = notice.lines().join("\n");
    assert!(
        text.contains("Converted, so hidden here: Invoicing"),
        "{text}"
    );
    assert!(
        text.contains("Old format, needs conversion: one-offs"),
        "{text}"
    );
    // The old folder is still there, untouched.
    assert!(layout::task_file(&project).exists());
}

#[gpui::test]
async fn the_conversion_explanation_shows_once(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let store = new_store(cx, WorkDirs::for_test(root.clone()));
    // The test database is shared: start from an explanation never shown.
    let db = cx.update(|cx| db::kvp::KeyValueStore::global(cx));
    db.scoped(crate::KVP_NAMESPACE)
        .delete(CONVERSION_NOTICE_KEY.to_string())
        .await
        .unwrap();
    let restart = |cx: &mut TestAppContext| {
        let fs = store.read_with(cx, |store, _| store.fs());
        let dirs = WorkDirs::for_test(root.clone());
        cx.new(|cx| WorkStore::new(fs, dirs, cx))
    };
    let first = restart(cx);
    enable_project_repositories(cx);
    wait_until(cx, &first, "the explanation", |store| {
        store
            .conversion_notice()
            .is_some_and(|notice| notice.explanation == Some(CONVERSION_EXPLANATION))
    })
    .await;
    cx.run_until_parked();
    let stored = db
        .scoped(crate::KVP_NAMESPACE)
        .read(CONVERSION_NOTICE_KEY)
        .unwrap();
    assert!(stored.is_some(), "it is remembered as shown");

    // A later run names the old projects but does not explain again.
    let later = restart(cx);
    wait_until(cx, &later, "the notice after a restart", |store| {
        store.conversion_notice().is_some()
    })
    .await;
    let notice = later.read_with(cx, |store, _| store.conversion_notice().unwrap());
    assert_eq!(notice.explanation, None);
    assert_eq!(notice.needs_conversion, ["Invoicing", "one-offs"]);

    // Dismissing hides it in the run that showed it.
    first.update(cx, |store, cx| store.dismiss_conversion_notice(cx));
    let notice = first.read_with(cx, |store, _| store.conversion_notice().unwrap());
    assert_eq!(notice.explanation, None);
    // Nothing was converted by any of this.
    assert!(layout::task_file(&layout::project_dir(&root, "invoicing")).exists());
    assert!(!canonical(&temp).join("projects").exists());
}

#[gpui::test]
async fn the_oneoffs_project_setting_has_a_default_and_takes_a_name(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        ProjectRepositorySettings::register(cx);
    });
    let title = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            ProjectRepositorySettings::get_global(cx)
                .oneoffs_project
                .clone()
        })
    };
    assert_eq!(title(cx), DEFAULT_ONEOFFS_TITLE);
    assert_eq!(title(cx), "One-offs");
    for (set, expected) in [("Side quests", "Side quests"), ("   ", "One-offs")] {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |settings, cx| {
                settings.update_user_settings(cx, |content| {
                    content
                        .project_repository
                        .get_or_insert_default()
                        .oneoffs_project = Some(set.to_string());
                });
            });
        });
        assert_eq!(title(cx), expected);
    }
}

fn git() -> Git {
    Git::locate().expect("git is installed").isolated()
}

fn setup(base: &Path) -> ConversionSetup {
    ConversionSetup {
        root: base.join("work"),
        locations_path: base.join("locations.toml"),
        projects_dir: base.join("projects"),
        layout: LayoutTemplates::default(),
        oneoffs_title: "One-offs".to_string(),
        now: "2026-11-13T10:00:00Z".parse().unwrap(),
    }
}

#[test]
fn project_convert_from_the_preview_to_the_hidden_project() {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let setup = setup(&base);
    let project = seed(&setup.root);
    let before = contents(&setup.root);

    let found = previews(&setup).unwrap();
    assert_eq!(found.len(), 2, "the project and the one-offs");
    let text = render_previews(&found);
    for expected in [
        "The old folder is only read",
        "Invoicing (",
        "2 tasks",
        "Will make:",
        "the project folder",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from\n{text}");
    }

    // Work starting in the project holds it: the conversion refuses and makes nothing.
    let claim = claim_old_project_start(&project).unwrap();
    let results =
        futures::executor::block_on(run_conversion(&setup, Some(&project), &git())).unwrap();
    assert!(matches!(results[0].1, Err(ConvertError::Busy(_))));
    assert!(conversion_message(&results).starts_with("Not converted"));
    assert!(!setup.projects_dir.join("invoicing").exists());
    drop(claim);

    let results =
        futures::executor::block_on(run_conversion(&setup, Some(&project), &git())).unwrap();
    let message = conversion_message(&results);
    assert!(message.starts_with("Converted "), "{message}");
    let converted = results[0].1.as_ref().unwrap();
    assert_eq!(
        converted.location.converted_from.as_deref(),
        Some(&*project)
    );
    assert!(converted.location.folder.starts_with(&setup.projects_dir));
    assert_eq!(contents(&setup.root), before, "the old folder is only read");

    // The Work view leaves it out from now on; the one-offs still need converting.
    let locations = RepositoryLocations::load(&setup.locations_path).unwrap();
    let mut tree = load(&setup.root);
    let hidden = hide_converted(&mut tree, Some(&locations));
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0].name, "Invoicing");
    assert!(tree.projects.is_empty());
    assert_eq!(tree.activities.len(), 1);
    assert_eq!(previews(&setup).unwrap().len(), 1);

    // Converting it again is refused.
    let error =
        futures::executor::block_on(run_conversion(&setup, Some(&project), &git())).unwrap_err();
    assert!(
        error.to_string().contains("still needs converting"),
        "{error}"
    );
}

// One-offs -------------------------------------------------------------------------------------

fn new_format_task(id: &str, dir: PathBuf, definition: &str, children: Vec<Task>) -> Task {
    let mut file = TaskFile::new(id, Kind::Other);
    file.id = id.to_string();
    file.todos = Vec::new();
    Task {
        slug: id.to_string(),
        dir,
        definition: Some(PathBuf::from(definition)),
        pending: None,
        needs_conversion: false,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: Default::default(),
        hashes: Default::default(),
        children,
    }
}

/// The one-offs project `top`, with an activity group `general` when `with_group`.
fn oneoffs_tree(with_group: bool) -> WorkTree {
    let files = PathBuf::from("/machine/projects/one-offs/files");
    let mut children = Vec::new();
    if with_group {
        let mut group = new_format_task("grp", files.join("grp"), "tasks/general", Vec::new());
        group.slug = "general".to_string();
        let leaf = new_format_task("old", files.join("old"), "tasks/general/tasks/old", vec![]);
        group.children.push(leaf);
        group.file.as_mut().unwrap().activity = Some("general".to_string());
        children.push(group);
    }
    WorkTree {
        root: PathBuf::from("/work"),
        projects: vec![Project {
            slug: "one-offs".to_string(),
            file: Some(ProjectFile::new("One-offs")),
            task: new_format_task("top", files.join("top"), "", children),
        }],
        ..WorkTree::default()
    }
}

fn definition_paths(routed: Routed) -> Vec<String> {
    let Routed::Definition { definition, .. } = routed else {
        panic!("a new task changes the definitions");
    };
    definition
        .into_iter()
        .filter_map(|op| match op {
            DefinitionOp::Write { path, .. } => Some(String::from(path)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_oneoff_goes_under_its_activity_group_made_once() {
    let new = || NewTask {
        goal: "Check the logs".to_string(),
        ..NewTask::default()
    };
    let tree = oneoffs_tree(false);
    assert!(oneoffs_project(&tree, "One-offs").is_some());
    assert!(oneoffs_project(&tree, "Side quests").is_none());

    // No group yet: the group and the one-off under it.
    let change = commands::new_oneoff_in_project(&tree, &ctx(), "top", "general", new()).unwrap();
    let files: Vec<&TaskFile> = change
        .writes
        .iter()
        .filter_map(|write| match write {
            Write::Task { file, .. } => Some(file),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].goal, "general");
    assert_eq!(files[0].activity.as_deref(), Some("general"));
    assert!(files[0].todos.is_empty());
    assert_eq!(files[1].goal, "Check the logs");
    assert_eq!(files[1].kind, Kind::Spike);
    assert_eq!(files[1].activity.as_deref(), Some("general"));
    assert_eq!(
        definition_paths(route(&tree, &change).unwrap()),
        [
            "tasks/general/task.toml",
            "tasks/general/tasks/check-the-logs/task.toml"
        ]
    );

    // With the group: only the one-off, a child of it.
    let tree = oneoffs_tree(true);
    let change = commands::new_oneoff_in_project(&tree, &ctx(), "top", "General", new()).unwrap();
    assert_eq!(change.writes.len(), 1);
    assert_eq!(
        definition_paths(route(&tree, &change).unwrap()),
        ["tasks/general/tasks/check-the-logs/task.toml"]
    );

    // Another activity gets its own group.
    let change = commands::new_oneoff_in_project(&tree, &ctx(), "top", "research", new()).unwrap();
    assert_eq!(
        definition_paths(route(&tree, &change).unwrap()),
        [
            "tasks/research/task.toml",
            "tasks/research/tasks/check-the-logs/task.toml"
        ]
    );
    assert!(commands::new_oneoff_in_project(&tree, &ctx(), "top", " ", new()).is_err());
}

fn oneoffs_request(base: &Path) -> bongos_project_repo::CreateRequest {
    bongos_project_repo::CreateRequest {
        title: "One-offs".to_string(),
        folder: base.join("projects").join("one-offs"),
        layout: LayoutTemplates::default(),
        repositories: Vec::new(),
        remote: None,
    }
}

#[test]
fn the_oneoffs_project_is_made_once() {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let record = base.join("locations.toml");
    let git = git();
    let ensure = || {
        let git = git.clone();
        futures::executor::block_on(ensure_oneoffs_project(
            oneoffs_request(&base),
            &record,
            move || Ok(git),
        ))
        .unwrap()
    };
    let (first, made) = ensure();
    assert!(made);
    assert!(first.folder.join("repository").exists());
    let (second, made) = ensure();
    assert!(!made, "the second one-off reuses it");
    assert_eq!(second.id, first.id);
    assert_eq!(
        RepositoryLocations::load(&record)
            .unwrap()
            .locations()
            .len(),
        1
    );
}

#[test]
fn one_offs_asked_for_at_once_make_one_project() {
    let temp = TempDir::new().unwrap();
    let base = canonical(&temp);
    let record = base.join("locations.toml");
    let git = git();
    let results: Vec<(Location, bool)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let git = git.clone();
                let base = &base;
                let record = &record;
                scope.spawn(move || {
                    futures::executor::block_on(ensure_oneoffs_project(
                        oneoffs_request(base),
                        record,
                        move || Ok(git),
                    ))
                    .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(results.iter().filter(|(_, made)| *made).count(), 1);
    assert!(
        results
            .iter()
            .all(|(location, _)| location.id == results[0].0.id)
    );
    assert_eq!(
        RepositoryLocations::load(&record)
            .unwrap()
            .locations()
            .len(),
        1
    );
}
