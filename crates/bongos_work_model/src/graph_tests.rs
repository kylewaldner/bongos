use crate::derive::{child_order, level_graph};
use crate::fixtures::*;
use crate::graph::GraphLine;
use crate::store::Task;
use crate::*;

fn order(parent: &Task) -> Vec<&str> {
    child_order(parent)
        .into_iter()
        .map(|task| task.slug.as_str())
        .collect()
}

#[test]
fn ties_break_by_created_then_slug() {
    let g = group(
        "g",
        vec![
            with_created(leaf("a", Status::Open), "2026-11-03T00:00:00Z"),
            with_created(leaf("b", Status::Open), "2026-11-01T00:00:00Z"),
            with_created(leaf("c", Status::Open), "2026-11-01T00:00:00Z"),
        ],
    );
    assert_eq!(order(&g), ["b", "c", "a"]);
}

#[test]
fn edges_order_before_created() {
    // x was created first but comes after y; z, created between, goes ahead of y.
    let g = group(
        "g",
        vec![
            after(
                with_created(leaf("x", Status::Open), "2026-11-01T00:00:00Z"),
                "y",
                UnblockPoint::Merged,
            ),
            with_created(leaf("y", Status::Open), "2026-11-03T00:00:00Z"),
            with_created(leaf("z", Status::Open), "2026-11-02T00:00:00Z"),
            stacked_on(
                with_created(leaf("w", Status::Open), "2026-11-01T12:00:00Z"),
                "x",
                UnblockPoint::Implemented,
            ),
        ],
    );
    assert_eq!(order(&g), ["z", "y", "x", "w"]);
}

#[test]
fn cycles_and_broken_edges_do_not_panic() {
    let g = group(
        "g",
        vec![
            after(
                with_created(leaf("a", Status::Open), "2026-11-02T00:00:00Z"),
                "b",
                UnblockPoint::Merged,
            ),
            after(
                with_created(leaf("b", Status::Open), "2026-11-01T00:00:00Z"),
                "a",
                UnblockPoint::Merged,
            ),
            after(
                with_created(leaf("c", Status::Open), "2026-11-03T00:00:00Z"),
                "missing",
                UnblockPoint::Merged,
            ),
            after(
                with_created(leaf("d", Status::Open), "2026-11-04T00:00:00Z"),
                "d",
                UnblockPoint::Merged,
            ),
        ],
    );
    // c and d are ready first; the cycle's leftovers follow by created.
    assert_eq!(order(&g), ["c", "d", "b", "a"]);
}

fn lines(graph: &[GraphLine]) -> Vec<String> {
    graph.iter().map(|line| line.text.clone()).collect()
}

#[test]
fn graph_of_a_linear_list() {
    let t = tree(vec![
        with_created(pr_ship("a"), "2026-11-01T00:00:00Z"),
        after(
            with_created(pr_ship("b"), "2026-11-02T00:00:00Z"),
            "a",
            UnblockPoint::Review,
        ),
        after(
            with_created(pr_ship("c"), "2026-11-03T00:00:00Z"),
            "b",
            UnblockPoint::Merged,
        ),
    ]);
    let graph = level_graph(&t.projects[0].task, &t);
    assert_eq!(
        lines(&graph),
        [
            "+------------+",
            "| a  Ready   |",
            "+------------+",
            "  v  after (review) from a",
            "+------------+",
            "| b  Blocked |",
            "+------------+",
            "  v  after (merged) from b",
            "+------------+",
            "| c  Blocked |",
            "+------------+",
        ]
    );
    // Box lines carry the task id; arrow lines do not.
    let nodes: Vec<Option<TaskId>> = graph.iter().map(|line| line.node.clone()).collect();
    let (a, b, c) = (id_of(&t, "a"), id_of(&t, "b"), id_of(&t, "c"));
    let some = |id: &TaskId| Some(id.clone());
    assert_eq!(
        nodes,
        [
            some(&a),
            some(&a),
            some(&a),
            None,
            some(&b),
            some(&b),
            some(&b),
            None,
            some(&c),
            some(&c),
            some(&c),
        ]
    );
}

#[test]
fn graph_of_a_stack_tree() {
    let t = tree(vec![
        with_created(pr_ship("base"), "2026-11-01T00:00:00Z"),
        stacked_on(
            with_created(pr_ship("one"), "2026-11-02T00:00:00Z"),
            "base",
            UnblockPoint::Implemented,
        ),
        stacked_on(
            with_created(pr_ship("two"), "2026-11-03T00:00:00Z"),
            "base",
            UnblockPoint::Implemented,
        ),
    ]);
    let graph = level_graph(&t.projects[0].task, &t);
    assert_eq!(
        lines(&graph),
        [
            "+--------------+",
            "| base  Ready  |",
            "+--------------+",
            "  v  stacked (implemented) from base",
            "+--------------+",
            "| one  Blocked |",
            "+--------------+",
            "  v  stacked (implemented) from base",
            "+--------------+",
            "| two  Blocked |",
            "+--------------+",
        ]
    );
    assert_eq!(graph.iter().filter(|line| line.node.is_none()).count(), 2);
}
