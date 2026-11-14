use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::derive::*;
use crate::store::{Project, Task, WorkTree};
use crate::*;

const AT: &str = "2026-11-08T10:05:00Z";

fn todo(name: &str, role: Option<TodoRole>, state: TodoState, started: bool) -> Todo {
    Todo {
        id: None,
        name: name.to_string(),
        role,
        workflow: None,
        state,
        started: started.then(|| AT.parse().unwrap()),
        extra: Extra::new(),
    }
}

fn pr_result(state: ResultState) -> TaskResult {
    TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://github.com/acme/invoicing/pull/1".to_string()),
        state: Some(state),
        urls: Vec::new(),
        extra: Extra::new(),
    }
}

/// A `pr-ship` leaf with todos implement, review and ship, none started.
fn pr_ship(slug: &str) -> Task {
    let mut file = TaskFile::new(slug, Kind::PrShip);
    file.todos = vec![
        todo(
            "implement",
            Some(TodoRole::ProducesBranch),
            TodoState::Open,
            false,
        ),
        todo("review", None, TodoState::Open, false),
        todo("ship", Some(TodoRole::Ships), TodoState::Open, false),
    ];
    task(slug, file)
}

fn task(slug: &str, file: TaskFile) -> Task {
    Task {
        slug: slug.to_string(),
        dir: PathBuf::from(slug),
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        definition: None,
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    }
}

fn group(slug: &str, children: Vec<Task>) -> Task {
    let mut task = task(slug, TaskFile::new(slug, Kind::Other));
    task.children = children;
    task
}

fn file_mut(task: &mut Task) -> &mut TaskFile {
    task.file.as_mut().unwrap()
}

fn after(slug: &str, unblock: UnblockPoint) -> Edge {
    Edge {
        task: slug.to_string(),
        unblock,
        extra: Extra::new(),
    }
}

fn stacked(slug: &str, unblock: UnblockPoint) -> StackedOn {
    StackedOn {
        task: slug.to_string(),
        unblock,
        extra: Extra::new(),
    }
}

fn place(task: &mut Task, parent: &Path) {
    task.dir = parent.join(&task.slug);
    let dir = task.dir.clone();
    for child in &mut task.children {
        place(child, &dir.join("tasks"));
    }
}

/// A one-project tree whose top group holds `children`.
fn tree(children: Vec<Task>) -> WorkTree {
    let mut top = group("p", children);
    place(&mut top, Path::new("/w/projects"));
    WorkTree {
        root: PathBuf::from("/w"),
        projects: vec![Project {
            slug: "p".to_string(),
            file: None,
            task: top,
        }],
        ..WorkTree::default()
    }
}

fn find<'a>(tree: &'a WorkTree, slug: &str) -> &'a Task {
    tree.tasks()
        .into_iter()
        .find(|task| task.slug == slug)
        .unwrap()
}

fn state_of(tree: &WorkTree, slug: &str) -> State {
    state(find(tree, slug), tree)
}

/// State of a lone `pr-ship` leaf after `edit`.
fn pr_ship_state(edit: impl FnOnce(&mut TaskFile)) -> State {
    let mut leaf = pr_ship("a");
    edit(file_mut(&mut leaf));
    state_of(&tree(vec![leaf]), "a")
}

fn start_implement(file: &mut TaskFile) {
    file.todos[0].started = Some(AT.parse().unwrap());
}

fn finish_implement(file: &mut TaskFile) {
    start_implement(file);
    file.todos[0].state = TodoState::Done;
}

#[test]
fn shape_is_group_with_children_else_leaf() {
    let t = tree(vec![group("g", vec![pr_ship("a")]), pr_ship("b")]);
    assert_eq!(shape(find(&t, "g"), &t), Shape::Group);
    assert_eq!(shape(find(&t, "a"), &t), Shape::Leaf);
    assert_eq!(shape(find(&t, "p"), &t), Shape::Group);
    let empty = tree(Vec::new());
    assert_eq!(shape(find(&empty, "p"), &empty), Shape::Group);
}

#[test]
fn stackable_needs_both_roles_whatever_the_names() {
    let mut leaf = pr_ship("a");
    assert!(stackable(&leaf));
    file_mut(&mut leaf).todos[0].name = "write the code".to_string();
    file_mut(&mut leaf).todos[2].name = "land it".to_string();
    assert!(stackable(&leaf));

    let mut no_ships = pr_ship("b");
    file_mut(&mut no_ships).todos.pop();
    assert!(!stackable(&no_ships));
    let mut no_produces = pr_ship("c");
    file_mut(&mut no_produces).todos.remove(0);
    assert!(!stackable(&no_produces));
    assert!(!stackable(&group("g", Vec::new())));
}

#[test]
fn current_todo_and_leaf_done_ignore_cancelled_and_skipped() {
    let mut leaf = pr_ship("a");
    assert_eq!(current_todo(&leaf).unwrap().name, "implement");
    assert!(!leaf_done(&leaf));
    let todos = &mut file_mut(&mut leaf).todos;
    todos[0].state = TodoState::Done;
    todos[1].state = TodoState::Skipped;
    assert_eq!(current_todo(&leaf).unwrap().name, "ship");
    file_mut(&mut leaf).todos[2].state = TodoState::Cancelled;
    assert!(current_todo(&leaf).is_none());
    assert!(leaf_done(&leaf));
    assert!(!leaf_done(&task("e", TaskFile::new("e", Kind::Other))));
}

#[test]
fn pr_ship_ready_in_progress_review_implemented() {
    assert_eq!(pr_ship_state(|_| {}), State::Ready);
    // A worktree alone is work begun.
    assert_eq!(
        pr_ship_state(|file| file.worktree = Some("/wt".to_string())),
        State::InProgress
    );
    assert_eq!(pr_ship_state(start_implement), State::InProgress);
    // An open PR does not pull a task out of In Progress while it is still implementing.
    assert_eq!(
        pr_ship_state(|file| {
            start_implement(file);
            file.result = Some(pr_result(ResultState::Open));
        }),
        State::InProgress
    );
    // Implement done, review not started.
    assert_eq!(pr_ship_state(finish_implement), State::Implemented);
    assert_eq!(
        pr_ship_state(|file| {
            finish_implement(file);
            file.result = Some(pr_result(ResultState::Closed));
        }),
        State::Implemented
    );
    // A later todo started.
    assert_eq!(
        pr_ship_state(|file| {
            finish_implement(file);
            file.todos[1].started = Some(AT.parse().unwrap());
        }),
        State::Review
    );
    // A PR result open or approved.
    for result in [ResultState::Open, ResultState::Approved] {
        assert_eq!(
            pr_ship_state(|file| {
                finish_implement(file);
                file.result = Some(pr_result(result));
            }),
            State::Review
        );
    }
}

#[test]
fn pr_ship_merged_done_and_cancelled() {
    // Merged PR with the ships todo still open.
    assert_eq!(
        pr_ship_state(|file| {
            finish_implement(file);
            file.result = Some(pr_result(ResultState::Merged));
        }),
        State::Merged
    );
    // Merged wins over a still-open implement todo (first match).
    assert_eq!(
        pr_ship_state(|file| file.result = Some(pr_result(ResultState::Merged))),
        State::Merged
    );
    // Ships todo done: no open todo left.
    assert_eq!(
        pr_ship_state(|file| {
            for todo in &mut file.todos {
                todo.state = TodoState::Done;
            }
            file.result = Some(pr_result(ResultState::Merged));
        }),
        State::Done
    );
    // Skipped and cancelled todos never keep a task open.
    assert_eq!(
        pr_ship_state(|file| {
            file.todos[0].state = TodoState::Done;
            file.todos[1].state = TodoState::Skipped;
            file.todos[2].state = TodoState::Cancelled;
        }),
        State::Done
    );
    assert_eq!(
        pr_ship_state(|file| file.status = Status::Done),
        State::Done
    );
    assert_eq!(
        pr_ship_state(|file| {
            start_implement(file);
            file.status = Status::Cancelled;
        }),
        State::Cancelled
    );
}

#[test]
fn pr_merge_is_done_when_the_pr_is_merged() {
    let mut leaf = pr_ship("a");
    file_mut(&mut leaf).kind = Kind::PrMerge;
    file_mut(&mut leaf).result = Some(pr_result(ResultState::Merged));
    let t = tree(vec![leaf]);
    assert_eq!(state_of(&t, "a"), State::Done);

    let mut open = pr_ship("b");
    file_mut(&mut open).kind = Kind::PrMerge;
    file_mut(&mut open).result = Some(pr_result(ResultState::Approved));
    finish_implement(file_mut(&mut open));
    let t = tree(vec![open]);
    assert_eq!(state_of(&t, "b"), State::Review);
}

#[test]
fn other_kinds_use_the_shorter_order() {
    let mut spike = TaskFile::new("s", Kind::Spike);
    spike.todos = vec![todo("investigate", None, TodoState::Open, false)];
    let ready = task("s", spike.clone());
    assert_eq!(state_of(&tree(vec![ready]), "s"), State::Ready);

    spike.todos[0].started = Some(AT.parse().unwrap());
    assert_eq!(
        state_of(&tree(vec![task("s", spike.clone())]), "s"),
        State::InProgress
    );

    // A PR result means nothing for a spike.
    spike.result = Some(pr_result(ResultState::Merged));
    assert_eq!(
        state_of(&tree(vec![task("s", spike.clone())]), "s"),
        State::InProgress
    );
    spike.todos[0].state = TodoState::Done;
    assert_eq!(
        state_of(&tree(vec![task("s", spike.clone())]), "s"),
        State::Done
    );
    spike.status = Status::Cancelled;
    assert_eq!(
        state_of(&tree(vec![task("s", spike)]), "s"),
        State::Cancelled
    );
}

#[test]
fn a_leaf_with_no_todos_is_not_done() {
    let t = tree(vec![task("a", TaskFile::new("a", Kind::PrShip))]);
    assert_eq!(state_of(&t, "a"), State::Ready);
}

#[test]
fn renaming_the_implement_todo_changes_nothing() {
    let mut leaf = pr_ship("a");
    finish_implement(file_mut(&mut leaf));
    let mut renamed = leaf.clone();
    renamed.slug = "b".to_string();
    for todo in &mut file_mut(&mut renamed).todos {
        todo.name = format!("renamed {}", todo.name);
    }
    file_mut(&mut renamed).todos[0].name = "Write the migration".to_string();
    assert!(stackable(&leaf) && stackable(&renamed));
    assert_eq!(state(&leaf, &tree(vec![leaf.clone()])), State::Implemented);

    // Unblocking follows the role, not the title: both targets reached Implemented.
    for (name, target) in [("a", leaf), ("b", renamed)] {
        let mut later = pr_ship("later");
        file_mut(&mut later).stacked_on = Some(stacked(name, UnblockPoint::Implemented));
        let t = tree(vec![target, later]);
        assert!(!blocked(find(&t, "later"), &t), "{name}");
    }

    // A renamed todo that is still open keeps the target unreached.
    let mut open = pr_ship("open");
    file_mut(&mut open).todos[0].name = "implement".to_string();
    let mut dependent = pr_ship("dep");
    file_mut(&mut dependent).stacked_on = Some(stacked("open", UnblockPoint::Implemented));
    let t = tree(vec![open, dependent]);
    assert!(blocked(find(&t, "dep"), &t));
}

fn blocked_by(target: impl FnOnce(&mut TaskFile), edge: Edge) -> bool {
    let mut first = pr_ship("first");
    target(file_mut(&mut first));
    let mut second = pr_ship("second");
    file_mut(&mut second).after = vec![edge];
    let t = tree(vec![first, second]);
    blocked(find(&t, "second"), &t)
}

fn to_implemented(file: &mut TaskFile) {
    finish_implement(file);
}

fn to_review(file: &mut TaskFile) {
    finish_implement(file);
    file.result = Some(pr_result(ResultState::Open));
}

fn to_merged(file: &mut TaskFile) {
    finish_implement(file);
    file.result = Some(pr_result(ResultState::Merged));
}

#[test]
fn after_edge_unblocks_at_each_point() {
    let stages: [(&str, fn(&mut TaskFile)); 4] = [
        ("started", start_implement),
        ("implemented", to_implemented),
        ("review", to_review),
        ("merged", to_merged),
    ];
    // For each point, which stages have reached it.
    let table = [
        (UnblockPoint::Implemented, [false, true, true, true]),
        (UnblockPoint::Review, [false, false, true, true]),
        (UnblockPoint::Merged, [false, false, false, true]),
    ];
    for (point, expected) in table {
        for ((name, stage), want_unblocked) in stages.iter().zip(expected) {
            let is_blocked = blocked_by(stage, after("first", point));
            assert_eq!(is_blocked, !want_unblocked, "{point:?} at {name}");
        }
    }
    // Done reaches every point, as does an untouched target for none.
    assert!(!blocked_by(
        |file| file.status = Status::Done,
        after("first", UnblockPoint::Merged)
    ));
    assert!(blocked_by(
        |_| {},
        after("first", UnblockPoint::Implemented)
    ));
}

#[test]
fn stacked_on_edge_unblocks_at_each_point() {
    let stages: [fn(&mut TaskFile); 3] = [to_implemented, to_review, to_merged];
    for (point, first_reached) in [
        (UnblockPoint::Implemented, 0),
        (UnblockPoint::Review, 1),
        (UnblockPoint::Merged, 2),
    ] {
        for (index, stage) in stages.iter().enumerate() {
            let mut first = pr_ship("first");
            stage(file_mut(&mut first));
            let mut second = pr_ship("second");
            file_mut(&mut second).stacked_on = Some(stacked("first", point));
            let t = tree(vec![first, second]);
            assert_eq!(
                blocked(find(&t, "second"), &t),
                index < first_reached,
                "{point:?} at stage {index}"
            );
        }
    }
}

#[test]
fn state_is_blocked_before_ready_and_in_progress_but_after_merged() {
    let mut first = pr_ship("first");
    let mut second = pr_ship("second");
    file_mut(&mut second).after = vec![after("first", UnblockPoint::Merged)];
    let t = tree(vec![first.clone(), second.clone()]);
    assert_eq!(state_of(&t, "second"), State::Blocked);
    assert_eq!(state_of(&t, "first"), State::Ready);

    start_implement(file_mut(&mut second));
    let t = tree(vec![first.clone(), second.clone()]);
    assert_eq!(state_of(&t, "second"), State::Blocked);

    // Once the target is merged the badge clears with no other change.
    to_merged(file_mut(&mut first));
    let t = tree(vec![first, second.clone()]);
    assert_eq!(state_of(&t, "second"), State::InProgress);

    // A merged PR shows Merged even if an edge is unreached.
    let mut lone = pr_ship("lone");
    file_mut(&mut lone).after = vec![after("missing-ok", UnblockPoint::Merged)];
    let mut gate = pr_ship("missing-ok");
    file_mut(&mut gate).todos.truncate(1);
    to_merged(file_mut(&mut lone));
    start_implement(file_mut(&mut gate));
    let t = tree(vec![gate, lone]);
    assert_eq!(state_of(&t, "lone"), State::Merged);
}

#[test]
fn blocking_inherits_downward() {
    let mut first = pr_ship("first");
    start_implement(file_mut(&mut first));
    let mut inner = group(
        "inner",
        vec![pr_ship("child"), pr_ship("grandchild-holder")],
    );
    file_mut(&mut inner).after = vec![after("first", UnblockPoint::Merged)];
    let t = tree(vec![first, inner]);
    assert!(blocked(find(&t, "inner"), &t));
    assert!(blocked(find(&t, "child"), &t));
    assert!(blocked(find(&t, "grandchild-holder"), &t));
    assert_eq!(state_of(&t, "child"), State::Blocked);
    assert_eq!(state_of(&t, "inner"), State::Blocked);
    assert!(!blocked(find(&t, "first"), &t));
    assert!(!blocked(find(&t, "p"), &t));

    // Deeper: a group inside the blocked group.
    let mut first = pr_ship("first");
    start_implement(file_mut(&mut first));
    let mut outer = group("outer", vec![group("mid", vec![pr_ship("deep")])]);
    file_mut(&mut outer).after = vec![after("first", UnblockPoint::Review)];
    let t = tree(vec![first, outer]);
    assert!(blocked(find(&t, "deep"), &t));
    // A group's state is Blocked only by its own edges.
    assert_eq!(state_of(&t, "mid"), State::Ready);
    assert_eq!(state_of(&t, "outer"), State::Blocked);
}

#[test]
fn group_edge_target_needs_every_non_cancelled_leaf() {
    let mut done = pr_ship("done");
    file_mut(&mut done).status = Status::Done;
    let mut cancelled = pr_ship("cancelled");
    file_mut(&mut cancelled).status = Status::Cancelled;
    let mut pending = pr_ship("pending");
    to_review(file_mut(&mut pending));
    let mut dependent = pr_ship("dependent");
    file_mut(&mut dependent).after = vec![after("g", UnblockPoint::Merged)];
    let target = group("g", vec![done.clone(), cancelled.clone(), pending]);
    let t = tree(vec![target, dependent.clone()]);
    assert!(blocked(find(&t, "dependent"), &t));

    file_mut(&mut dependent).after = vec![after("g", UnblockPoint::Review)];
    let mut pending = pr_ship("pending");
    to_review(file_mut(&mut pending));
    let t = tree(vec![
        group("g", vec![done.clone(), cancelled.clone(), pending]),
        dependent.clone(),
    ]);
    assert!(!blocked(find(&t, "dependent"), &t));

    // Only cancelled and done leaves left: reached.
    file_mut(&mut dependent).after = vec![after("g", UnblockPoint::Merged)];
    let t = tree(vec![group("g", vec![done, cancelled]), dependent]);
    assert!(!blocked(find(&t, "dependent"), &t));
}

#[test]
fn non_pr_target_reaches_every_point_when_done() {
    let mut spike = TaskFile::new("spike", Kind::Spike);
    spike.todos = vec![todo("look", None, TodoState::Open, true)];
    let mut dependent = pr_ship("dependent");
    file_mut(&mut dependent).after = vec![after("spike", UnblockPoint::Implemented)];
    let t = tree(vec![task("spike", spike.clone()), dependent.clone()]);
    assert!(blocked(find(&t, "dependent"), &t));
    spike.todos[0].state = TodoState::Done;
    let t = tree(vec![task("spike", spike), dependent]);
    assert!(!blocked(find(&t, "dependent"), &t));
}

#[test]
fn cancelled_after_target_counts_as_reached() {
    let mut first = pr_ship("first");
    file_mut(&mut first).status = Status::Cancelled;
    let mut second = pr_ship("second");
    file_mut(&mut second).after = vec![after("first", UnblockPoint::Merged)];
    let t = tree(vec![first, second]);
    assert!(!blocked(find(&t, "second"), &t));
    assert!(!retarget_needed(find(&t, "second"), &t));
    assert_eq!(state_of(&t, "second"), State::Ready);

    // A group whose children are all cancelled is cancelled too.
    let mut inner = pr_ship("inner");
    file_mut(&mut inner).status = Status::Cancelled;
    let mut dependent = pr_ship("dependent");
    file_mut(&mut dependent).after = vec![after("g", UnblockPoint::Merged)];
    let t = tree(vec![group("g", vec![inner]), dependent]);
    assert!(!blocked(find(&t, "dependent"), &t));
}

#[test]
fn cancelled_stacked_on_target_blocks_until_retargeted() {
    for point in [
        UnblockPoint::Implemented,
        UnblockPoint::Review,
        UnblockPoint::Merged,
    ] {
        let mut first = pr_ship("first");
        to_merged(file_mut(&mut first));
        file_mut(&mut first).status = Status::Cancelled;
        let mut second = pr_ship("second");
        file_mut(&mut second).stacked_on = Some(stacked("first", point));
        let t = tree(vec![first, second]);
        assert!(blocked(find(&t, "second"), &t), "{point:?}");
        assert!(retarget_needed(find(&t, "second"), &t));
        assert_eq!(state_of(&t, "second"), State::Blocked);
    }

    // Retargeting (a new stacked_on) clears it.
    let mut cancelled = pr_ship("first");
    file_mut(&mut cancelled).status = Status::Cancelled;
    let mut other = pr_ship("other");
    to_review(file_mut(&mut other));
    let mut second = pr_ship("second");
    file_mut(&mut second).stacked_on = Some(stacked("other", UnblockPoint::Review));
    let t = tree(vec![cancelled, other, second]);
    assert!(!blocked(find(&t, "second"), &t));
    assert!(!retarget_needed(find(&t, "second"), &t));
}

#[test]
fn broken_edges_do_not_block() {
    let mut lone = pr_ship("lone");
    file_mut(&mut lone).after = vec![after("nobody", UnblockPoint::Merged)];
    file_mut(&mut lone).stacked_on = Some(stacked("nobody-either", UnblockPoint::Review));
    lone.broken_edges = vec!["nobody".to_string(), "nobody-either".to_string()];
    let t = tree(vec![lone]);
    assert!(!blocked(find(&t, "lone"), &t));
    assert!(!retarget_needed(find(&t, "lone"), &t));
    assert_eq!(state_of(&t, "lone"), State::Ready);
    // The flag is still there for whoever shows it.
    assert_eq!(find(&t, "lone").broken_edges.len(), 2);

    // A sibling of another parent is not a sibling: the edge is broken too.
    let mut inner = pr_ship("inner");
    file_mut(&mut inner).after = vec![after("outside", UnblockPoint::Merged)];
    let t = tree(vec![pr_ship("outside"), group("g", vec![inner])]);
    assert!(!blocked(find(&t, "inner"), &t));
}

#[test]
fn group_states() {
    let mut done = pr_ship("done");
    file_mut(&mut done).status = Status::Done;
    let mut cancelled = pr_ship("cancelled");
    file_mut(&mut cancelled).status = Status::Cancelled;
    let mut started = pr_ship("started");
    start_implement(file_mut(&mut started));

    let cases: [(Vec<Task>, State); 7] = [
        (Vec::new(), State::Ready),
        (vec![pr_ship("a"), pr_ship("b")], State::Ready),
        (vec![pr_ship("a"), started.clone()], State::InProgress),
        (vec![done.clone(), cancelled.clone()], State::Done),
        (vec![done.clone(), pr_ship("a")], State::InProgress),
        (vec![cancelled.clone(), cancelled.clone()], State::Cancelled),
        (vec![cancelled, started], State::InProgress),
    ];
    for (index, (children, want)) in cases.into_iter().enumerate() {
        let mut children = children;
        // Unique slugs for repeated children.
        for (n, child) in children.iter_mut().enumerate() {
            child.slug = format!("{}-{index}-{n}", child.slug);
        }
        let t = tree(vec![group("g", children)]);
        assert_eq!(state_of(&t, "g"), want, "case {index}");
    }

    // A group of done groups is done; a started descendant three levels down counts.
    let mut deep = pr_ship("deep");
    start_implement(file_mut(&mut deep));
    let t = tree(vec![group("g", vec![group("h", vec![deep])])]);
    assert_eq!(state_of(&t, "g"), State::InProgress);
    assert_eq!(state_of(&t, "h"), State::InProgress);

    // A merged-but-not-shipped child keeps the group from Done.
    let mut merged = pr_ship("merged");
    to_merged(file_mut(&mut merged));
    let t = tree(vec![group("g", vec![merged])]);
    assert_eq!(state_of(&t, "g"), State::InProgress);
}

#[test]
fn group_blocked_by_its_own_edges() {
    let mut first = pr_ship("first");
    start_implement(file_mut(&mut first));
    let mut g = group("g", vec![pr_ship("child")]);
    file_mut(&mut g).after = vec![after("first", UnblockPoint::Merged)];
    let t = tree(vec![first.clone(), g.clone()]);
    assert_eq!(state_of(&t, "g"), State::Blocked);

    to_merged(file_mut(&mut first));
    let t = tree(vec![first, g]);
    assert_eq!(state_of(&t, "g"), State::Ready);
}

#[test]
fn derived_values_are_not_part_of_the_file() {
    let mut leaf = pr_ship("a");
    finish_implement(file_mut(&mut leaf));
    let before = serialize_task(leaf.file.as_ref().unwrap()).unwrap();
    let t = tree(vec![leaf]);
    let _ = (
        state_of(&t, "a"),
        blocked(find(&t, "a"), &t),
        stackable(find(&t, "a")),
    );
    let after_text = serialize_task(find(&t, "a").file.as_ref().unwrap()).unwrap();
    assert_eq!(before, after_text);
    assert!(!after_text.contains("implemented"));
}

/// A leaf of `kind` whose todos are a plan (role `plans`) and then `implement`.
fn planned_leaf(kind: Kind, plan: TodoState, plan_started: bool) -> Task {
    let mut file = TaskFile::new("p", kind);
    let produces = matches!(kind, Kind::PrShip | Kind::PrMerge).then_some(TodoRole::ProducesBranch);
    file.todos = vec![
        todo("plan", Some(TodoRole::Plans), plan, plan_started),
        todo("implement", produces, TodoState::Open, false),
    ];
    task("p", file)
}

const ALL_KINDS: [Kind; 6] = [
    Kind::PrShip,
    Kind::PrMerge,
    Kind::Document,
    Kind::Spike,
    Kind::Babysit,
    Kind::Other,
];

#[test]
fn a_done_plans_todo_leaves_the_leaf_ready() {
    for kind in ALL_KINDS {
        // Done, with and without a start time.
        for started in [false, true] {
            let leaf = planned_leaf(kind, TodoState::Done, started);
            assert_eq!(state_of(&tree(vec![leaf]), "p"), State::Ready, "{kind:?}");
        }
        // Planning skipped or cancelled does not start the leaf either.
        for plan in [TodoState::Skipped, TodoState::Cancelled] {
            let leaf = planned_leaf(kind, plan, true);
            assert_eq!(state_of(&tree(vec![leaf]), "p"), State::Ready, "{kind:?}");
        }
    }
}

#[test]
fn an_open_plans_todo_is_in_progress_only_once_started() {
    for kind in ALL_KINDS {
        let not_started = planned_leaf(kind, TodoState::Open, false);
        assert_eq!(
            state_of(&tree(vec![not_started]), "p"),
            State::Ready,
            "{kind:?}"
        );
        let started = planned_leaf(kind, TodoState::Open, true);
        assert_eq!(
            state_of(&tree(vec![started]), "p"),
            State::InProgress,
            "{kind:?}"
        );
    }
}

#[test]
fn work_after_a_done_plans_todo_still_starts_the_leaf() {
    for kind in ALL_KINDS {
        let mut leaf = planned_leaf(kind, TodoState::Done, true);
        file_mut(&mut leaf).todos[1].started = Some(AT.parse().unwrap());
        assert_eq!(
            state_of(&tree(vec![leaf]), "p"),
            State::InProgress,
            "{kind:?}"
        );
    }
    let mut leaf = planned_leaf(Kind::PrShip, TodoState::Done, true);
    file_mut(&mut leaf).worktree = Some("/wt".to_string());
    assert_eq!(state_of(&tree(vec![leaf]), "p"), State::InProgress);
}

#[test]
fn a_group_with_only_planned_leaves_is_ready() {
    let leaf = planned_leaf(Kind::PrShip, TodoState::Done, true);
    let t = tree(vec![group("g", vec![leaf])]);
    assert_eq!(state_of(&t, "g"), State::Ready);
}
