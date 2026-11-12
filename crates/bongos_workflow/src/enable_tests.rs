//! Tests that workflows being on by default starts nothing and changes no task: under the default
//! settings, with tasks on disk and no runs, initializing the engine and ticking it starts no run
//! and no process, writes nothing under `runs/`, and leaves every task file and `context.md`
//! byte for byte as it was. A child of the lifecycle tests, so it shares their world and scripted
//! runner.

use std::collections::BTreeMap;

use super::task_link_tests::linked;
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

    // The settings are the shipped defaults, and the engine is built the way `ensure` builds it
    // (its sources, gates and observers registered) over this world's directories and a scripted
    // runner, so it sees the task tree and the `runs/` root. `crate::init` itself is not called:
    // it would write the shipped definitions into the real data directory.
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        WorkflowsSettings::register(cx);
        assert!(WorkflowsSettings::get_global(cx).enabled);
    });
    let linked = linked(&world, FakeRunner::new(), cx);
    cx.update(|cx| {
        cx.set_global(linked.engine.clone());
        crate::sources::register(&linked.engine, cx);
        crate::gates::register(&linked.engine, cx);
        crate::run_view::observe_focus(&linked.engine, cx);
    });

    // Ticking, settling and publishing find nothing to start.
    linked.drive(cx);
    cx.update(|cx| linked.engine.publish(cx));
    cx.run_until_parked();
    {
        let lifecycle = linked.engine.lifecycle();
        assert!(lifecycle.all_runs().is_empty(), "no run exists");
        assert!(RunRow::all(&lifecycle).is_empty());
    }
    {
        let runner = linked.runner.lock().unwrap();
        assert!(runner.started.is_empty(), "no process was started");
        assert!(runner.in_flight().is_empty());
    }
    assert!(linked.engine.active_run(LEAF).is_none());
    assert!(cx.update(|cx| LiveModesRegistry::can_start_run(LEAF, cx).allowed));
    assert!(
        linked.writes.changes.lock().unwrap().is_empty(),
        "no task write was made"
    );

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
