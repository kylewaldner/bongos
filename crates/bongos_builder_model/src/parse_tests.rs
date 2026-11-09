use bongos_work_model::rules::EdgeKind;
use bongos_work_model::{Kind, TodoRole, UnblockPoint};

use crate::grammar::{By, Located, Severity};
use crate::help::FORMAT;
use crate::parse;

const EXAMPLE: &str = "\
# bongos task proposal. Lines starting with # are comments. (format help: D10, about 30 lines)
proposal 1
for 0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33
by chat 0196f2aa-1d3e-7f00-8c11-0a9b8c7d6e5f
why billing needs proration; one PR per layer

task billing: implement proration for billing | id 0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33
  task schema: add proration columns to plans
    result a PR adding proration columns
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task engine: proration engine
    stacked on schema
    result a PR with the proration engine
    todo implement | role produces-branch
    todo review
    todo ship | role ships
  task integration: use proration in invoices
    after engine
    result a PR wiring proration into invoice totals
  task api: proration endpoint
    after integration
    result a PR adding the endpoint
  task deploy: deploy proration | kind other
    after schema | unblock merged
    after engine
    after integration
    after api
    result proration live in production
    todo deploy
";

fn problems(text: &str) -> Vec<(usize, String)> {
    parse(text)
        .problems
        .into_iter()
        .map(|problem| (problem.line, problem.message))
        .collect()
}

fn single(text: &str) -> (usize, String) {
    let found = problems(text);
    assert_eq!(found.len(), 1, "{found:?}");
    found.into_iter().next().unwrap_or_default()
}

fn error(line: usize, message: &str) -> (usize, String) {
    (line, message.to_string())
}

#[test]
fn example_parses_without_problems() {
    let parsed = parse(EXAMPLE);
    assert_eq!(parsed.problems, vec![]);
    assert!(!parsed.has_errors());
    assert_eq!(parsed.header.number, Some(Located { value: 1, line: 2 }));
    assert_eq!(
        parsed
            .header
            .for_task
            .as_ref()
            .map(|task| task.value.as_str()),
        Some("0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33")
    );
    assert_eq!(
        parsed.header.by,
        Some(Located {
            value: By::Chat("0196f2aa-1d3e-7f00-8c11-0a9b8c7d6e5f".to_string()),
            line: 4
        })
    );
    assert_eq!(
        parsed.header.why,
        Some(Located {
            value: "billing needs proration; one PR per layer".to_string(),
            line: 5
        })
    );

    let root = &parsed.root;
    assert_eq!((root.line, root.label.as_str()), (7, "billing"));
    assert_eq!(root.goal, "implement proration for billing");
    assert_eq!(
        root.attributes.id.as_deref(),
        Some("0196f1c2-7c1e-7a8b-9d55-2f6c0e1a4b33")
    );
    let labels: Vec<_> = root
        .children
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(labels, ["schema", "engine", "integration", "api", "deploy"]);

    let schema = &root.children[0];
    assert_eq!(schema.line, 8);
    assert_eq!(schema.results[0].value, "a PR adding proration columns");
    assert_eq!(schema.results[0].line, 9);
    let todos: Vec<_> = schema
        .todos
        .iter()
        .map(|todo| (todo.name.as_str(), todo.role, todo.line))
        .collect();
    assert_eq!(
        todos,
        [
            ("implement", Some(TodoRole::ProducesBranch), 10),
            ("review", None, 11),
            ("ship", Some(TodoRole::Ships), 12),
        ]
    );

    let engine = &root.children[1];
    assert_eq!(engine.edges.len(), 1);
    assert_eq!(engine.edges[0].kind, EdgeKind::StackedOn);
    assert_eq!(engine.edges[0].target, ["schema"]);
    assert_eq!(engine.edges[0].unblock, None);
    assert_eq!(engine.edges[0].line, 14);

    let deploy = &root.children[4];
    assert_eq!(deploy.attributes.kind, Some(Kind::Other));
    let edges: Vec<_> = deploy
        .edges
        .iter()
        .map(|edge| (edge.kind, edge.target[0].as_str(), edge.unblock))
        .collect();
    assert_eq!(
        edges,
        [
            (EdgeKind::After, "schema", Some(UnblockPoint::Merged)),
            (EdgeKind::After, "engine", None),
            (EdgeKind::After, "integration", None),
            (EdgeKind::After, "api", None),
        ]
    );
}

#[test]
fn comments_blank_lines_and_trailing_whitespace_are_skipped() {
    let parsed = parse(
        "# first\r\n\r\nproposal 4   \r\n    # an odd comment\r\n\t# a tabbed comment\r\n  \r\n\
         task a: goal   \r\n  todo x  \r\n",
    );
    assert_eq!(parsed.problems, vec![]);
    assert_eq!(parsed.header.number.map(|number| number.value), Some(4));
    assert_eq!(parsed.root.goal, "goal");
    assert_eq!(parsed.root.todos[0].name, "x");
}

#[test]
fn header_lines_are_all_optional_but_proposal() {
    let parsed = parse("proposal 7\nby kyle\nwhy\ntask a: goal\n");
    assert_eq!(parsed.problems, vec![]);
    assert_eq!(parsed.header.for_task, None);
    assert_eq!(parsed.header.by.map(|by| by.value), Some(By::Kyle));
    assert_eq!(parsed.header.why.map(|why| why.value), Some(String::new()));

    let parsed = parse("proposal 1\nby workflow plan it\ntask a: goal\n");
    assert_eq!(parsed.problems, vec![]);
    assert_eq!(
        parsed.header.by.map(|by| by.value),
        Some(By::Workflow("plan it".to_string()))
    );
}

#[test]
fn task_and_todo_attributes_parse() {
    let parsed = parse(
        "proposal 1\ntask a: goal | id x1 | kind spike | keeps work | state in-progress\n  \
         task b: other | drop\n  todo w | workflow review | done | started\n",
    );
    assert_eq!(parsed.problems, vec![]);
    let attributes = &parsed.root.attributes;
    assert_eq!(attributes.id.as_deref(), Some("x1"));
    assert_eq!(attributes.kind, Some(Kind::Spike));
    assert!(attributes.keeps_work && !attributes.drop);
    assert_eq!(attributes.state.as_deref(), Some("in-progress"));
    assert!(parsed.root.children[0].attributes.drop);
    let todo = &parsed.root.todos[0];
    assert_eq!(todo.workflow.as_deref(), Some("review"));
    assert!(todo.started);
    assert_eq!(todo.mark, Some(crate::grammar::TodoMark::Done));
}

#[test]
fn the_plans_role_and_paths_parse() {
    let parsed = parse(
        "proposal 1\ntask a: goal\n  todo plan | role plans\n  after billing/schema | unblock review\n",
    );
    assert_eq!(parsed.problems, vec![]);
    assert_eq!(parsed.root.todos[0].role, Some(TodoRole::Plans));
    assert_eq!(parsed.root.edges[0].target, ["billing", "schema"]);
    assert_eq!(parsed.root.edges[0].unblock, Some(UnblockPoint::Review));
}

#[test]
fn two_results_are_both_kept_for_validation() {
    let parsed = parse("proposal 1\ntask a: goal\n  result one\n  result two\n");
    assert_eq!(parsed.problems, vec![]);
    let lines: Vec<_> = parsed
        .root
        .results
        .iter()
        .map(|result| result.line)
        .collect();
    assert_eq!(lines, [3, 4]);
}

#[test]
fn tab_in_indentation() {
    assert_eq!(
        single("proposal 1\ntask a: g\n\ttodo x\n"),
        error(3, "tab in indentation: indent with two spaces per level")
    );
}

#[test]
fn odd_indentation() {
    assert_eq!(
        single("proposal 1\ntask a: g\n   todo x\n"),
        error(3, "indentation of 3 spaces is not a multiple of two")
    );
}

#[test]
fn indentation_jump() {
    assert_eq!(
        single("proposal 1\ntask a: g\n      todo x\n"),
        error(
            3,
            "indentation too deep: expected at most 2 spaces, found 6"
        )
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  todo x\n    todo y\n"),
        error(
            4,
            "indentation too deep: expected at most 2 spaces, found 4"
        )
    );
}

#[test]
fn indented_line_before_the_root() {
    assert_eq!(
        problems("proposal 1\n  task a: g\n"),
        vec![
            error(2, "indented line before the first task"),
            error(
                2,
                "no task: a proposal has one top-level 'task <label>: <goal>' line"
            ),
        ]
    );
}

#[test]
fn header_out_of_order() {
    assert_eq!(
        single("proposal 1\nby kyle\nfor x\ntask a: g\n"),
        error(
            3,
            "header line 'for' is out of order: the order is format, proposal, for, by, why"
        )
    );
    assert_eq!(
        single("for x\nproposal 1\ntask a: g\n"),
        error(
            2,
            "header line 'proposal' is out of order: the order is format, proposal, for, by, why"
        )
    );
    assert_eq!(
        single("proposal 1\ntask a: g\nwhy late\n"),
        error(3, "header line 'why' must come before the first task")
    );
}

#[test]
fn repeated_header_line() {
    assert_eq!(
        single("proposal 1\nfor x\nfor y\ntask a: g\n"),
        error(3, "repeated header line 'for'")
    );
    assert_eq!(
        single("proposal 1\nproposal 2\ntask a: g\n"),
        error(2, "repeated header line 'proposal'")
    );
}

#[test]
fn unknown_kind() {
    assert_eq!(
        single("proposal 1\ntask a: g | kind weird\n"),
        error(
            2,
            "unknown kind 'weird': expected pr-ship, pr-merge, document, spike, babysit or other"
        )
    );
}

#[test]
fn unknown_attributes() {
    assert_eq!(
        single("proposal 1\ntask a: g | shape round\n"),
        error(2, "unknown task attribute 'shape round'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  todo x | speed fast\n"),
        error(3, "unknown todo attribute 'speed fast'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  after b | when late\n"),
        error(3, "unknown edge attribute 'when late'")
    );
}

#[test]
fn unknown_role_and_unblock_point() {
    assert_eq!(
        single("proposal 1\ntask a: g\n  todo x | role boss\n"),
        error(
            3,
            "unknown role 'boss': expected produces-branch, ships or plans"
        )
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  after b | unblock never\n"),
        error(
            3,
            "unknown unblock point 'never': expected implemented, review or merged"
        )
    );
}

#[test]
fn repeated_attributes() {
    assert_eq!(
        single("proposal 1\ntask a: g | id x | id y\n"),
        error(2, "task attribute 'id' is repeated")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  todo x | done | skipped\n"),
        error(3, "a todo has at most one of done, skipped and cancelled")
    );
}

#[test]
fn format_versions() {
    assert_eq!(
        single("format 2\nproposal 1\ntask a: g\n"),
        error(
            1,
            "unsupported format version 2: this bongos reads format 1"
        )
    );
    assert_eq!(
        single("format one\nproposal 1\ntask a: g\n"),
        error(1, "bad format version 'one': write 'format 1'")
    );
    assert_eq!(parse("format 1\nproposal 1\ntask a: g\n").problems, vec![]);
    assert_eq!(
        single("proposal 1\nformat 1\ntask a: g\n"),
        error(
            2,
            "header line 'format' is out of order: the order is format, proposal, for, by, why"
        )
    );
}

#[test]
fn missing_or_bad_proposal_line() {
    assert_eq!(
        single("task a: g\n"),
        error(1, "missing 'proposal N' line before the first task")
    );
    assert_eq!(
        single("proposal\ntask a: g\n"),
        error(1, "'proposal' needs a number, as in 'proposal 3'")
    );
    for bad in ["x", "0", "-1", "1.5", "+2"] {
        assert_eq!(
            single(&format!("proposal {bad}\ntask a: g\n")),
            error(
                1,
                &format!(
                    "bad proposal number '{bad}': write a whole number from 1, as in 'proposal 3'"
                )
            )
        );
    }
}

#[test]
fn bad_labels() {
    let message = "use lowercase letters, digits and '-', starting with a letter or digit";
    for bad in ["Billing", "-a", "a_b", "my label", ""] {
        assert_eq!(
            single(&format!("proposal 1\ntask {bad}: g\n")),
            error(2, &format!("bad label '{bad}': {message}"))
        );
    }
    assert_eq!(
        single("proposal 1\ntask a: g\n  after b/C\n"),
        error(3, &format!("bad label 'C' in 'after b/C': {message}"))
    );
}

#[test]
fn malformed_task_and_item_lines() {
    assert_eq!(
        single("proposal 1\ntask a goal\n"),
        error(
            2,
            "a task line is 'task <label>: <goal>', with a colon after the label"
        )
    );
    assert_eq!(
        single("proposal 1\ntask a:goal\n"),
        error(2, "task 'a': write a space after the colon")
    );
    assert_eq!(
        single("proposal 1\ntask a:\n"),
        error(2, "task 'a' has no goal")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  todo\n"),
        error(3, "'todo' needs a name, as in 'todo implement'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  result\n"),
        error(3, "'result' needs text, as in 'result a PR adding X'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  stacked b\n"),
        error(3, "expected 'stacked on <task>'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  stacked on\n"),
        error(3, "'stacked on' needs a task, as in 'stacked on schema'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  after\n"),
        error(3, "'after' needs a task, as in 'after schema'")
    );
    assert_eq!(
        single("proposal 1\ntask a: g\n  bogus x\n"),
        error(
            3,
            "unknown line 'bogus': expected task, result, todo, after or stacked on"
        )
    );
}

#[test]
fn second_root_and_missing_root() {
    assert_eq!(
        single("proposal 1\ntask a: g\ntask b: h\n  todo x\n"),
        error(
            3,
            "second top-level task 'b': a proposal has exactly one root task"
        )
    );
    let (line, message) = single("proposal 1\n# nothing else\n");
    assert_eq!(line, 2);
    assert_eq!(
        message,
        "no task: a proposal has one top-level 'task <label>: <goal>' line"
    );
    let parsed = parse("");
    assert_eq!(parsed.root.line, 0);
    assert_eq!(
        problems(""),
        vec![
            error(1, "missing 'proposal N' line"),
            error(
                1,
                "no task: a proposal has one top-level 'task <label>: <goal>' line"
            ),
        ]
    );
}

#[test]
fn bad_for_and_by_lines() {
    assert_eq!(
        single("proposal 1\nfor\ntask a: g\n"),
        error(2, "'for' needs one task id, as in 'for <id>'")
    );
    assert_eq!(
        single("proposal 1\nby bot\ntask a: g\n"),
        error(
            2,
            "bad 'by' line 'bot': write 'by kyle', 'by chat <id>' or 'by workflow <name>'"
        )
    );
}

#[test]
fn every_problem_has_a_line_and_is_an_error() {
    let parsed = parse("proposal x\nfor\ntask A: g\n");
    assert!(parsed.problems.iter().all(|problem| problem.line >= 1));
    assert!(
        parsed
            .problems
            .iter()
            .all(|problem| problem.severity == Severity::Error)
    );
}

#[test]
fn parsing_continues_after_an_error() {
    let parsed = parse(
        "\
proposal 1
task root: r | id r1
  task Bad Label: x
    todo skipped under the bad label
    todo another | role nope
  task good: g
    todo implement | role ships | bogus
    after nothing/else
  task other: o | kind weird
    result still here
  task last: l
\ttodo tabbed
    todo under the tab
  task final: f
    todo ok
",
    );
    let found: Vec<_> = parsed
        .problems
        .iter()
        .map(|problem| (problem.line, problem.message.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            (
                3,
                "bad label 'Bad Label': use lowercase letters, digits and '-', starting with a letter or digit"
            ),
            (7, "unknown todo attribute 'bogus'"),
            (
                9,
                "unknown kind 'weird': expected pr-ship, pr-merge, document, spike, babysit or other"
            ),
            (12, "tab in indentation: indent with two spaces per level"),
        ]
    );
    let labels: Vec<_> = parsed
        .root
        .children
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(labels, ["good", "other", "last", "final"]);
    let good = &parsed.root.children[0];
    assert_eq!(good.todos[0].role, Some(TodoRole::Ships));
    assert_eq!(good.edges[0].target, ["nothing", "else"]);
    assert_eq!(parsed.root.children[1].results[0].value, "still here");
    assert_eq!(parsed.root.children[3].todos[0].name, "ok");
    assert!(parsed.root.children[2].todos.is_empty());
}

#[test]
fn a_bad_root_line_skips_its_subtree() {
    let parsed = parse("proposal 1\ntask Bad: g\n  todo x\n  task child: c\n");
    assert_eq!(parsed.problems.len(), 1);
    assert_eq!(parsed.root.line, 0);
}

#[test]
fn format_help_leads_a_proposal() {
    assert!(FORMAT.lines().any(|line| line == "format 1"));
    let parsed = parse(&format!("{FORMAT}proposal 1\ntask a: goal\n"));
    assert_eq!(parsed.problems, vec![]);
    assert_eq!(parsed.header.format.map(|format| format.value), Some(1));
    let only_help = parse(FORMAT);
    assert_eq!(
        only_help
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>(),
        [
            "missing 'proposal N' line",
            "no task: a proposal has one top-level 'task <label>: <goal>' line"
        ]
    );
    assert!(FORMAT.lines().count() <= 35);
}
