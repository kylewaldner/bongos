//! The single-file commands: what each writes, its history entry, the write guard, and writing a
//! change to disk.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::commands::*;
use crate::fixtures::*;
use crate::layout;
use crate::rules::EdgeKind;
use crate::store::{WorkTree, load, reload};
use crate::*;

fn goal(text: &str) -> NewTask {
    NewTask {
        goal: text.to_string(),
        ..NewTask::default()
    }
}

fn accept(tree: &mut WorkTree, command: impl FnOnce(&WorkTree) -> Outcome) -> Change {
    let change = command(tree).expect("the command should be accepted");
    update(tree, &change);
    change
}

fn todo_states(tree: &WorkTree, slug: &str) -> Vec<TodoState> {
    find(tree, slug)
        .file
        .as_ref()
        .unwrap()
        .todos
        .iter()
        .map(|todo| todo.state)
        .collect()
}

#[test]
fn new_child_under_a_group_gets_the_default_todos_and_a_history_entry() {
    let t = tree(vec![pr_ship("a")]);
    let p = id_of(&t, "p");
    let change = new_child(&t, &ctx(), &p, goal("Backfill usage events")).unwrap();
    assert_eq!(
        change.dir,
        PathBuf::from("/w/projects/p/tasks/backfill-usage-events")
    );
    assert_eq!(change.writes.len(), 1);
    let file = change.file().unwrap();
    assert_eq!(file.goal, "Backfill usage events");
    assert_eq!(file.kind, Kind::PrShip);
    assert_eq!(file.created, ctx().at);
    let names: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
    assert_eq!(names, ["implement", "review", "ship"]);
    assert_eq!(file.history, vec![change.entry.clone()]);
    assert_eq!(change.entry.event, "created");
    assert_eq!(change.entry.by, "person");

    // A sibling with the same goal gets a suffix.
    let t = tree(vec![pr_ship("backfill")]);
    let change = new_child(&t, &ctx(), &id_of(&t, "p"), goal("Backfill")).unwrap();
    assert!(change.dir.ends_with("tasks/backfill-2"));
    assert_eq!(
        refusal(new_child(&t, &ctx(), &id_of(&t, "p"), goal("  "))),
        "a task needs a goal"
    );
}

fn refusal(outcome: Outcome) -> String {
    outcome.expect_err("the command should be refused")
}

#[test]
fn new_child_on_a_plain_leaf_turns_it_into_a_group() {
    let t = tree(vec![pr_ship("a")]);
    let change = new_child(&t, &ctx(), &id_of(&t, "a"), goal("Part one")).unwrap();
    assert_eq!(
        change.dir,
        PathBuf::from("/w/projects/p/tasks/a/tasks/part-one")
    );
    let parent = change
        .task_file(Path::new("/w/projects/p/tasks/a"))
        .unwrap();
    assert!(parent.todos.is_empty());
    let entry = parent.history.last().unwrap();
    assert_eq!(entry.event, "child added");
    assert_eq!(
        entry.detail.as_deref(),
        Some("part-one; became a group, todos removed: implement, review, ship")
    );
}

#[test]
fn new_oneoff_is_filed_by_activity_and_date() {
    let mut t = tree(vec![]);
    t.activities = vec![activity(
        "support",
        vec![task(
            "2026-11-08-fix-login",
            TaskFile::new("Fix login", Kind::Spike),
        )],
    )];
    let change = new_oneoff(&t, &ctx(), "Support", goal("Fix login")).unwrap();
    assert_eq!(
        change.dir,
        PathBuf::from("/w/oneoffs/support/2026-11-08-fix-login-2")
    );
    let file = change.file().unwrap();
    assert_eq!(file.kind, Kind::Spike);
    assert_eq!(file.activity.as_deref(), Some("Support"));
    assert_eq!(
        refusal(new_oneoff(&t, &ctx(), " ", goal("x"))),
        "a one-off needs an activity"
    );
    let via_new = new_task(
        &t,
        &ctx(),
        Placement::OneOff {
            activity: "review".to_string(),
        },
        goal("Read PR"),
    )
    .unwrap();
    assert_eq!(
        via_new.dir,
        PathBuf::from("/w/oneoffs/review/2026-11-08-read-pr")
    );
}

#[test]
fn todo_commands_change_the_list_and_record_history() {
    let mut t = tree(vec![pr_ship("a")]);
    let a = id_of(&t, "a");

    let change = accept(&mut t, |t| todo_start(t, &ctx(), &a, None));
    assert_eq!(change.entry.event, "todo started");
    assert_eq!(
        find(&t, "a").file.as_ref().unwrap().todos[0].started,
        Some(ctx().at)
    );
    assert_eq!(
        refusal(todo_start(&t, &ctx(), &a, Some(0))),
        "todo implement has already started"
    );

    let change = accept(&mut t, |t| advance(t, &ctx(), &a, false));
    assert_eq!(change.entry.event, "todo done");
    assert_eq!(change.entry.detail.as_deref(), Some("implement"));
    accept(&mut t, |t| todo_skip(t, &ctx(), &a, None));
    assert_eq!(
        todo_states(&t, "a"),
        [TodoState::Done, TodoState::Skipped, TodoState::Open]
    );
    accept(&mut t, |t| todo_reopen(t, &ctx(), &a, 1));
    assert_eq!(
        refusal(todo_reopen(&t, &ctx(), &a, 1)),
        "todo review is already open"
    );

    let new = NewTodo {
        name: "deploy".to_string(),
        role: None,
        workflow: Some("deploy".to_string()),
    };
    accept(&mut t, |t| todo_add(t, &ctx(), &a, new, Some(3)));
    accept(&mut t, |t| todo_move(t, &ctx(), &a, 3, 0));
    accept(&mut t, |t| todo_set_workflow(t, &ctx(), &a, 0, None));
    accept(&mut t, |t| todo_remove(t, &ctx(), &a, 0));
    assert_eq!(refusal(todo_remove(&t, &ctx(), &a, 9)), "a has no todo 10");
    let file = find(&t, "a").file.clone().unwrap();
    let names: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
    assert_eq!(names, ["implement", "review", "ship"]);
    let events: Vec<&str> = file
        .history
        .iter()
        .map(|entry| entry.event.as_str())
        .collect();
    assert_eq!(
        events,
        [
            "todo started",
            "todo done",
            "todo skipped",
            "todo reopened",
            "todo added",
            "todo moved",
            "todo workflow set",
            "todo removed"
        ]
    );

    // Groups have no todos.
    assert_eq!(
        refusal(advance(&t, &ctx(), &id_of(&t, "p"), false)),
        "p is a group, and a group has no todos"
    );
}

#[test]
fn close_drop_and_reopen() {
    let mut t = tree(vec![pr_ship("a")]);
    let a = id_of(&t, "a");
    let change = accept(&mut t, |t| close(t, &ctx(), &a));
    let file = change.file().unwrap();
    assert_eq!(file.status, Status::Done);
    assert_eq!(file.closed, Some(ctx().at));
    assert_eq!(refusal(close(&t, &ctx(), &a)), "a is already done");
    assert_eq!(
        refusal(advance(&t, &ctx(), &a, false)),
        "a is done; reopen it first"
    );

    accept(&mut t, |t| reopen(t, &ctx(), &a));
    assert_eq!(find(&t, "a").file.as_ref().unwrap().closed, None);
    assert_eq!(refusal(reopen(&t, &ctx(), &a)), "a is already open");

    accept(&mut t, |t| drop_task(t, &ctx(), &a, false));
    assert_eq!(
        find(&t, "a").file.as_ref().unwrap().status,
        Status::Cancelled
    );
    assert_eq!(
        refusal(drop_task(&t, &ctx(), &a, false)),
        "a is already cancelled"
    );
}

#[test]
fn edit_refuses_a_goal_that_comes_from_linear() {
    let mut a = pr_ship("a");
    file_mut(&mut a).linear = Some("SEQ-121".to_string());
    file_mut(&mut a).linear_state = Some(LinearState::default());
    let t = tree(vec![a]);
    let a = id_of(&t, "a");
    let goal = |text: &str| TaskEdit {
        goal: Some(text.to_string()),
        ..TaskEdit::default()
    };
    assert_eq!(
        refusal(edit(&t, &ctx(), &a, goal("Renamed"))),
        "the title comes from Linear (SEQ-121); change it there or run task: open in linear"
    );
    // The same goal changes nothing, so it is not a refusal of the goal; kind and repository
    // stay editable.
    let change = edit(
        &t,
        &ctx(),
        &a,
        TaskEdit {
            goal: Some("a".to_string()),
            kind: Some(Kind::PrMerge),
            repository: Some(Some("billing".to_string())),
        },
    )
    .unwrap();
    let file = change.file().unwrap();
    assert_eq!(file.goal, "a");
    assert_eq!(file.kind, Kind::PrMerge);
    assert_eq!(file.repository.as_deref(), Some("billing"));
    assert!(file.linear_state.is_some());
    // A hand-set identifier alone does not lock the goal.
    let mut hand = pr_ship("b");
    file_mut(&mut hand).linear = Some("SEQ-9".to_string());
    let t = tree(vec![hand]);
    let b = id_of(&t, "b");
    assert!(edit(&t, &ctx(), &b, goal("Renamed")).is_ok());
}

#[test]
fn edit_changes_fields_and_keeps_the_slug() {
    let t = tree(vec![pr_ship("a")]);
    let a = id_of(&t, "a");
    let change = edit(
        &t,
        &ctx(),
        &a,
        TaskEdit {
            goal: Some("A better goal".to_string()),
            kind: Some(Kind::PrMerge),
            repository: Some(Some("billing".to_string())),
        },
    )
    .unwrap();
    assert_eq!(change.dir, find(&t, "a").dir);
    let file = change.file().unwrap();
    assert_eq!(file.goal, "A better goal");
    assert_eq!(file.kind, Kind::PrMerge);
    assert_eq!(
        change.entry.detail.as_deref(),
        Some(
            "goal: \"a\" -> \"A better goal\"; kind: pr-ship -> pr-merge; repository: inherited -> billing"
        )
    );
    assert_eq!(
        refusal(edit(&t, &ctx(), &a, TaskEdit::default())),
        "a already has those values"
    );
}

#[test]
fn edge_commands() {
    let mut t = tree(vec![pr_ship("a"), pr_ship("b"), pr_ship("c")]);
    let (a, b, c) = (id_of(&t, "a"), id_of(&t, "b"), id_of(&t, "c"));

    let change = accept(&mut t, |t| {
        add_edge(t, &ctx(), &b, &a, EdgeKind::StackedOn, None)
    });
    assert_eq!(
        change.entry.detail.as_deref(),
        Some("stacked on a (review)")
    );
    let change = accept(&mut t, |t| {
        add_edge(t, &ctx(), &c, &a, EdgeKind::After, None)
    });
    assert_eq!(change.entry.detail.as_deref(), Some("after a (merged)"));

    accept(&mut t, |t| {
        set_unblock_point(t, &ctx(), &b, "a", UnblockPoint::Implemented)
    });
    assert_eq!(
        find(&t, "b")
            .file
            .as_ref()
            .unwrap()
            .stacked_on
            .as_ref()
            .unwrap()
            .unblock,
        UnblockPoint::Implemented
    );
    assert_eq!(
        refusal(set_unblock_point(&t, &ctx(), &b, "c", UnblockPoint::Merged)),
        "b has no edge to c"
    );

    accept(&mut t, |t| clear_rebase(t, &ctx(), &b, "9ac0"));
    assert_eq!(
        find(&t, "b")
            .file
            .as_ref()
            .unwrap()
            .rebase_ack_sha
            .as_deref(),
        Some("9ac0")
    );
    assert_eq!(
        refusal(clear_rebase(&t, &ctx(), &c, "9ac0")),
        "c is not stacked on anything"
    );

    accept(&mut t, |t| set_merge_ship(t, &ctx(), &b, Some(false)));
    assert_eq!(
        find(&t, "b").file.as_ref().unwrap().merge_is_ship,
        Some(false)
    );

    // Retarget from a (say it was cancelled) onto c, then to main.
    let change = accept(&mut t, |t| {
        retarget(t, &ctx(), &b, Retarget::StackedOn(c.clone()))
    });
    assert_eq!(
        change.entry.detail.as_deref(),
        Some("from a to stacked on c (review)")
    );
    let file = find(&t, "b").file.clone().unwrap();
    assert_eq!(file.stacked_on.unwrap().task, "c");
    assert_eq!(file.rebase_ack_sha, None);
    accept(&mut t, |t| retarget(t, &ctx(), &b, Retarget::Main));
    assert!(find(&t, "b").file.as_ref().unwrap().stacked_on.is_none());

    accept(&mut t, |t| remove_edge(t, &ctx(), &c, "a"));
    assert!(find(&t, "c").file.as_ref().unwrap().after.is_empty());
    // A broken edge, naming no sibling, can be removed.
    let t = tree(vec![after(pr_ship("x"), "gone", UnblockPoint::Merged)]);
    assert!(remove_edge(&t, &ctx(), &id_of(&t, "x"), "gone").is_ok());
}

#[test]
fn every_command_refuses_a_task_with_an_invalid_file() {
    let t = tree(vec![
        badged(stacked_on(pr_ship("a"), "b", UnblockPoint::Review)),
        pr_ship("b"),
        badged(group("g", vec![pr_ship("x")])),
    ]);
    let (a, b) = (id_of(&t, "a"), id_of(&t, "b"));
    let expected = find(&t, "a").check_writable().unwrap_err().to_string();
    assert!(expected.contains("cannot be changed until its files are fixed"));
    let c = ctx();
    let result = TaskResult {
        kind: ResultKind::Pr,
        url: None,
        state: None,
        urls: Vec::new(),
        extra: Extra::new(),
    };
    let outcomes = [
        new_child(&t, &c, &a, goal("x")),
        new_task(&t, &c, Placement::Under(a.clone()), goal("x")),
        edit(
            &t,
            &c,
            &a,
            TaskEdit {
                goal: Some("y".to_string()),
                ..TaskEdit::default()
            },
        ),
        advance(&t, &c, &a, false),
        advance(&t, &c, &a, true),
        todo_skip(&t, &c, &a, None),
        todo_reopen(&t, &c, &a, 0),
        todo_add(
            &t,
            &c,
            &a,
            NewTodo {
                name: "x".to_string(),
                ..NewTodo::default()
            },
            None,
        ),
        todo_remove(&t, &c, &a, 0),
        todo_move(&t, &c, &a, 0, 1),
        todo_set_workflow(&t, &c, &a, 0, Some("w".to_string())),
        todo_set_role(&t, &c, &a, 1, None),
        todo_start(&t, &c, &a, None),
        close(&t, &c, &a),
        drop_task(&t, &c, &a, true),
        reopen(&t, &c, &a),
        set_result(&t, &c, &a, result),
        clear_result(&t, &c, &a),
        add_edge(&t, &c, &a, &b, EdgeKind::After, None),
        remove_edge(&t, &c, &a, "b"),
        set_unblock_point(&t, &c, &a, "b", UnblockPoint::Merged),
        set_merge_ship(&t, &c, &a, Some(false)),
        retarget(&t, &c, &a, Retarget::Main),
        clear_rebase(&t, &c, &a, "9ac0"),
    ];
    for (index, outcome) in outcomes.into_iter().enumerate() {
        assert_eq!(refusal(outcome), expected, "command {index}");
    }

    // Dropping a group refuses when a descendant it would cancel is invalid.
    let t = tree(vec![group("g", vec![badged(pr_ship("x"))])]);
    assert_eq!(
        refusal(drop_task(&t, &c, &id_of(&t, "g"), true)),
        find(&t, "x").check_writable().unwrap_err().to_string()
    );

    // Errors in `project.toml` are badges on the top group, and refuse the project commands.
    let mut t = tree(vec![]);
    t.projects[0].task = badged(t.projects[0].task.clone());
    let expected = t.projects[0].task.check_writable().unwrap_err().to_string();
    assert_eq!(refusal(project_archive(&t, &c, "p")), expected);
    assert_eq!(refusal(project_unarchive(&t, &c, "p")), expected);
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn project_commands_on_disk() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = load(root);
    let new = NewProject {
        title: "Invoicing".to_string(),
        repositories: vec!["invoicing".to_string()],
    };
    let change = project_new(&tree, &ctx(), new).unwrap();
    apply(root, &change).unwrap();
    let dir = layout::project_dir(root, "invoicing");
    assert_eq!(names_in(&dir), ["project.toml", "prompts.txt", "task.toml"]);
    assert_eq!(names_in(root), ["projects"]);
    assert!(!dir.join(".git").exists());

    let tree = reload(root, &tree);
    let project = &tree.projects[0];
    assert!(project.task.errors.is_empty());
    assert_eq!(project.file.as_ref().unwrap().repositories, ["invoicing"]);
    assert_eq!(project.task.goal(), "Invoicing");

    let change = project_archive(&tree, &ctx(), "invoicing").unwrap();
    apply(root, &change).unwrap();
    let tree = reload(root, &tree);
    assert!(tree.projects[0].file.as_ref().unwrap().archived);
    assert_eq!(
        refusal(project_archive(&tree, &ctx(), "invoicing")),
        "invoicing is already archived"
    );
    apply(
        root,
        &project_unarchive(&tree, &ctx(), "invoicing").unwrap(),
    )
    .unwrap();
    let tree = reload(root, &tree);
    assert!(!tree.projects[0].file.as_ref().unwrap().archived);
    let events: Vec<String> = tree.projects[0]
        .task
        .file
        .as_ref()
        .unwrap()
        .history
        .iter()
        .map(|entry| entry.event.clone())
        .collect();
    assert_eq!(
        events,
        ["project created", "project archived", "project unarchived"]
    );
    assert_eq!(names_in(&dir), ["project.toml", "prompts.txt", "task.toml"]);
}

#[test]
fn rename_keeps_paths() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = load(root);
    let new = NewProject {
        title: "Invoicing".to_string(),
        ..NewProject::default()
    };
    apply(root, &project_new(&tree, &ctx(), new).unwrap()).unwrap();
    let tree = reload(root, &tree);
    let project = tree.projects[0].task.id().unwrap().to_string();
    let change = new_child(&tree, &ctx(), &project, goal("Backfill usage events")).unwrap();
    apply(root, &change).unwrap();
    let dir = change.dir.clone();
    fs::create_dir_all(dir.join("notes")).unwrap();
    fs::write(dir.join("notes/plan.md"), "plan").unwrap();
    fs::create_dir_all(dir.join("chats")).unwrap();
    fs::write(dir.join("chats/one.md"), "chat").unwrap();

    let tree = reload(root, &tree);
    let id = change.file().unwrap().id.clone();
    let edit_goal = TaskEdit {
        goal: Some("Backfill every usage event since March".to_string()),
        ..TaskEdit::default()
    };
    let change = edit(&tree, &ctx(), &id, edit_goal).unwrap();
    assert_eq!(change.dir, dir);
    apply(root, &change).unwrap();

    let tree = reload(root, &tree);
    let task = tree.task_by_id(&id).unwrap();
    assert_eq!(task.dir, dir);
    assert_eq!(task.slug, "backfill-usage-events");
    assert_eq!(task.goal(), "Backfill every usage event since March");
    assert!(task.files.contains_key(Path::new("notes/plan.md")));
    assert!(task.files.contains_key(Path::new("chats/one.md")));
    assert_eq!(
        names_in(&layout::children_dir(&tree.projects[0].task.dir)),
        ["backfill-usage-events"]
    );
}

#[test]
fn several_files_are_written_through_a_journal() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = load(root);
    let new = NewProject {
        title: "Invoicing".to_string(),
        ..NewProject::default()
    };
    apply(root, &project_new(&tree, &ctx(), new).unwrap()).unwrap();
    let tree = reload(root, &tree);
    let project = tree.projects[0].task.id().unwrap().to_string();
    let group = new_child(&tree, &ctx(), &project, goal("Group")).unwrap();
    apply(root, &group).unwrap();
    let tree = reload(root, &tree);
    let group_id = group.file().unwrap().id.clone();
    // The plain leaf gains a child: two files, the child and the parent without its todos.
    let child = new_child(&tree, &ctx(), &group_id, goal("Child")).unwrap();
    assert_eq!(child.writes.len(), 2);
    apply(root, &child).unwrap();
    assert!(!layout::op_file(&group.dir).exists());
    let tree = reload(root, &tree);
    assert!(tree.tasks().iter().all(|task| task.errors.is_empty()));

    let change = drop_task(&tree, &ctx(), &group_id, true).unwrap();
    assert_eq!(change.writes.len(), 2);
    apply(root, &change).unwrap();
    let tree = reload(root, &tree);
    for task in tree.projects[0].task.walk().into_iter().skip(1) {
        assert_eq!(task.file.as_ref().unwrap().status, Status::Cancelled);
    }
    assert!(!layout::op_file(&group.dir).exists());
}

#[test]
fn history_beyond_the_limit_moves_to_history_old() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let tree = load(root);
    let new = NewProject {
        title: "P".to_string(),
        ..NewProject::default()
    };
    apply(root, &project_new(&tree, &ctx(), new).unwrap()).unwrap();
    let mut tree = reload(root, &tree);
    let id = tree.projects[0].task.id().unwrap().to_string();
    for round in 0..HISTORY_LIMIT + 4 {
        let value = Some(round % 2 == 0);
        apply(root, &set_merge_ship(&tree, &ctx(), &id, value).unwrap()).unwrap();
        tree = reload(root, &tree);
    }
    let dir = &tree.projects[0].task.dir;
    let history = &tree.projects[0].task.file.as_ref().unwrap().history;
    assert_eq!(history.len(), HISTORY_LIMIT);
    let old = read_history(&layout::history_old_file(dir)).unwrap();
    assert_eq!(old.history.len(), 5);
    assert_eq!(old.history[0].event, "project created");
    assert!(!layout::op_file(dir).exists());
}
