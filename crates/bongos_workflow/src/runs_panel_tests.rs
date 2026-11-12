//! Tests of the Runs panel's rows and filters. A child of the lifecycle tests, so it shares their
//! world, definitions and scripted runner. The panel being absent while `workflows.enabled` is
//! false is tested in the app, where the panel is added.

use bongos_work_model::store::Task;

use super::*;
use crate::lifecycle::RunStatus;
use crate::runs_panel::{Filters, RunRow};

const SECOND: &str = "leaf-two";

/// The world with a second task, `second`, with a worktree of its own.
fn two_task_world() -> World {
    let mut world = World::new();
    let worktree = world.root.join("other-worktree");
    fs::create_dir_all(&worktree).unwrap();
    let mut file = TaskFile::new("Add logout", Kind::PrShip);
    file.id = SECOND.to_string();
    file.branch = Some("task/logout".to_string());
    file.worktree = Some(worktree.to_string_lossy().into_owned());
    file.todos = vec![todo("implement", Some("single"), TodoState::Open)];
    let dir = world.root.join("work").join("app").join("tasks").join("logout");
    let leaf: Task = task(dir, file, Vec::new());
    world.tree.projects[0].task.children.push(leaf);
    world
}

fn start_task(
    engine: &WorkflowEngine,
    world: &World,
    task_id: &str,
    cx: &mut TestAppContext,
) -> String {
    let request = StartRequest {
        workflow: Some("single".to_string()),
        ..StartRequest::new(task_id)
    };
    let run = cx
        .update(|cx| engine.start_in(request, &world.tree, cx))
        .unwrap();
    for _ in 0..4 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }
    run
}

fn rows_of(engine: &WorkflowEngine) -> Vec<RunRow> {
    RunRow::all(&engine.lifecycle())
}

#[gpui::test]
fn runs_across_two_tasks_are_listed_with_state_and_cost(cx: &mut TestAppContext) {
    let world = two_task_world();
    let engine = engine_of(&world, cx);
    let first = start_task(&engine, &world, LEAF, cx);
    let second = start_task(&engine, &world, SECOND, cx);
    cx.update(|cx| engine.cancel(&second, cx)).unwrap();

    let rows = rows_of(&engine);
    assert_eq!(rows.len(), 2);
    let a = rows.iter().find(|row| row.run == first).unwrap();
    let b = rows.iter().find(|row| row.run == second).unwrap();
    assert_eq!((a.task_id.as_str(), b.task_id.as_str()), (LEAF, SECOND));
    assert_eq!(a.todo, "implement");
    assert_eq!(b.status, RunStatus::Cancelled);
    assert_ne!(a.status, RunStatus::Cancelled);
    assert_ne!(a.project, b.project);
    assert!(a.text().contains(a.status.label()), "{}", a.text());
    assert!(a.text().contains("$0.00"), "{}", a.text());

    // The active run sorts above the cancelled one.
    let shown = Filters::default().apply(&rows);
    assert_eq!(shown[0].run, first);
    assert_eq!(shown[1].run, second);
}

#[gpui::test]
fn rows_filter_by_task_project_and_state(cx: &mut TestAppContext) {
    let world = two_task_world();
    let engine = engine_of(&world, cx);
    let first = start_task(&engine, &world, LEAF, cx);
    let second = start_task(&engine, &world, SECOND, cx);
    cx.update(|cx| engine.cancel(&second, cx)).unwrap();
    let rows = rows_of(&engine);
    let runs = |filters: &Filters| -> Vec<String> {
        filters.apply(&rows).into_iter().map(|row| row.run).collect()
    };

    assert_eq!(runs(&Filters::default()).len(), 2);

    let mut by_task = Filters::default();
    by_task.cycle_task(&rows);
    assert_eq!(runs(&by_task).len(), 1);
    by_task.cycle_task(&rows);
    assert_eq!(runs(&by_task).len(), 1);
    by_task.cycle_task(&rows);
    assert_eq!(by_task.task, None, "cycles back to every task");
    by_task.task = Some(rows.iter().find(|r| r.run == second).unwrap().task.clone());
    assert_eq!(runs(&by_task), vec![second.clone()]);

    let mut by_project = Filters::default();
    by_project.project = Some(rows.iter().find(|r| r.run == first).unwrap().project.clone());
    assert_eq!(runs(&by_project), vec![first.clone()]);

    let by_state = Filters {
        state: Some(RunStatus::Cancelled),
        ..Filters::default()
    };
    assert_eq!(runs(&by_state), vec![second.clone()]);

    // Filters combine: the cancelled run is not in the first task.
    let both = Filters {
        task: by_task.task.clone(),
        state: Some(RunStatus::Running),
        ..Filters::default()
    };
    assert!(runs(&both).is_empty());

    let mut cycled = Filters::default();
    cycled.cycle_state();
    assert_eq!(cycled.state, Some(RunStatus::WaitingOnYou));
    assert!(cycled.text().unwrap().contains("waiting on you"));
}
