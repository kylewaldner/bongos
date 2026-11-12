//! Tests that workflows being on by default starts nothing and changes no task: under the default
//! settings, with tasks on disk and no runs, initializing the engine and ticking it starts no run
//! and no process, writes nothing under `runs/`, and leaves every task file and `context.md`
//! byte for byte as it was. A child of the lifecycle tests, so it shares their world and scripted
//! runner.

use std::collections::BTreeMap;

use super::*;
use crate::runs_panel::RunRow;

/// Every file under `dir` with its bytes, by path.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

#[gpui::test]
fn enabling_by_default_starts_nothing_and_changes_no_task(cx: &mut TestAppContext) {
    let world = World::new();
    let task_dir = world.tree.projects[0].task.children[0].dir.clone();
    fs::create_dir_all(&task_dir).unwrap();
    fs::write(
        task_dir.join("task.toml"),
        "id = \"leaf\"\ntitle = \"Add login\"\n",
    )
    .unwrap();
    fs::write(
        task_dir.join("context.md"),
        "# Context\n\nKeep this exactly.\n",
    )
    .unwrap();
    let before = snapshot(&world.root.join("work"));
    assert!(before.contains_key(&task_dir.join("context.md")));

    // The app's own init under the default settings: the engine is made, and nothing runs.
    let settings = cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
        let engine = cx
            .try_global::<WorkflowEngine>()
            .expect("the engine exists under the default settings")
            .clone();
        assert!(engine.lifecycle().all_runs().is_empty(), "no run exists");
        WorkflowsSettings::get_global(cx).clone()
    });
    assert!(settings.enabled);

    // The same settings over the scripted runner: ticking finds nothing to start.
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let mut lifecycle = Lifecycle::new(FakeRunner::new(), Box::new(worktrees), settings);
    settle(&mut lifecycle);
    assert!(lifecycle.all_runs().is_empty());
    assert!(RunRow::all(&lifecycle).is_empty());
    assert!(
        lifecycle.runtime().runner().started.is_empty(),
        "no process was started"
    );
    assert!(lifecycle.runtime().runner().in_flight().is_empty());

    assert!(!world.root.join("runs").exists(), "nothing under runs/");
    assert!(
        !task_dir.join("runs").exists(),
        "no task has a runs/ directory"
    );
    assert_eq!(
        snapshot(&world.root.join("work")),
        before,
        "tasks unchanged"
    );
}
