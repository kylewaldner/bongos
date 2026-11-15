use bongos_project_repo_model::{DefinitionTree, RelPath};

use crate::decl::Inherit;
use crate::effective::{Applicability, EffectiveSet, NamedSets, Row, Source, effective_set};

const RUST_SET: &str = include_str!("../test-fixtures/rust.toml");

const PROJECT: &str = "schema = 1\nid = \"p1\"\ntitle = \"Project\"\n";

fn named() -> NamedSets {
    NamedSets::parse([("checks/sets/rust.toml", RUST_SET)])
}

/// A tree from `(folder, id, kind, checks)`: the checks text is appended to the task's
/// `task.toml`, which is how a task's `checks` table is written.
fn tree(tasks: &[(&str, &str, &str, &str)]) -> DefinitionTree {
    let mut files = vec![(
        RelPath::new("project.toml").unwrap(),
        PROJECT.as_bytes().to_vec(),
    )];
    for (folder, id, kind, checks) in tasks {
        let path = if folder.is_empty() {
            "task.toml".to_string()
        } else {
            format!("{folder}/task.toml")
        };
        let text = format!(
            "schema = 1\nid = \"{id}\"\ngoal = \"{id}\"\nkind = \"{kind}\"\n\
             created = 2026-01-01T10:00:00Z\n\n{checks}\n"
        );
        files.push((RelPath::new(path).unwrap(), text.into_bytes()));
    }
    DefinitionTree::parse_files(files).unwrap()
}

fn set_of(task: &str, tree: &DefinitionTree, done: &[&str]) -> EffectiveSet {
    effective_set(task, tree, &named(), &|id| done.contains(&id))
}

fn names(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|row| row.decl.name.as_str()).collect()
}

fn row<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter()
        .find(|row| row.decl.name == name)
        .unwrap_or_else(|| panic!("no row {name} in {:?}", names(rows)))
}

const TOP: &str = "use = []\n";

/// A group `g` that uses the rust set and adds `lint`, with leaves below it.
fn project() -> DefinitionTree {
    tree(&[
        (
            "",
            "g",
            "other",
            "[checks]\nuse = [\"rust\"]\n\n[[checks.check]]\nname = \"lint\"\ncommand = [\"lint\"]\n",
        ),
        ("tasks/plain", "plain", "pr-ship", ""),
        (
            "tasks/override",
            "override",
            "pr-ship",
            "[[checks.check]]\nname = \"tests\"\ncommand = [\"own-tests\"]\n",
        ),
        (
            "tasks/drops",
            "drops",
            "pr-ship",
            "[checks]\ndrop = [\"lint\", \"nothing\"]\n",
        ),
        (
            "tasks/alone",
            "alone",
            "pr-ship",
            "[checks]\ninherit = \"none\"\n\n[[checks.check]]\nname = \"only\"\ncommand = [\"only\"]\n",
        ),
        ("tasks/mid", "mid", "other", ""),
        ("tasks/mid/tasks/deep", "deep", "pr-ship", ""),
    ])
}

#[test]
fn a_task_without_a_table_inherits_its_parents_rows() {
    let tree = project();
    let set = set_of("plain", &tree, &[]);
    assert_eq!(set.applicability, Applicability::Checks);
    assert!(!set.is_group);
    assert_eq!(names(&set.rows), ["format", "tests", "lint"]);
    for name in ["format", "tests", "lint"] {
        assert_eq!(
            row(&set.rows, name).source,
            Source::Inherited {
                from: "g".to_string()
            }
        );
    }
    assert!(
        set.closing.is_empty(),
        "a leaf judges no closing set of its own"
    );
    assert!(set.findings.is_empty(), "{:?}", set.findings);
}

#[test]
fn a_grandchild_inherits_from_the_task_that_declares_the_row() {
    let tree = project();
    let set = set_of("deep", &tree, &[]);
    assert_eq!(names(&set.rows), ["format", "tests", "lint"]);
    assert_eq!(
        row(&set.rows, "lint").source,
        Source::Inherited {
            from: "g".to_string()
        }
    );
}

#[test]
fn sets_come_with_file_check_and_line_where_known() {
    let tree = project();
    let group = set_of("g", &tree, &[]);
    assert!(group.is_group);

    let format = row(&group.rows, "format");
    assert_eq!(
        format.source,
        Source::Set {
            set: "rust".to_string()
        }
    );
    assert_eq!(format.declared.file, "checks/sets/rust.toml");
    assert_eq!(format.declared.check, "format");
    assert_eq!(
        format.declared.line,
        Some(1),
        "a named set is parsed from text"
    );
    assert_eq!(row(&group.rows, "tests").declared.line, Some(5));

    // The task's own table is read without spans: file and name, and no invented line.
    let lint = row(&group.rows, "lint");
    assert_eq!(lint.source, Source::Own);
    assert_eq!(lint.declared.file, "task.toml");
    assert_eq!(lint.declared.check, "lint");
    assert_eq!(lint.declared.line, None);

    // The closing set of the group comes from its set.
    assert_eq!(names(&group.closing), ["tests-full"]);
    assert_eq!(group.closing[0].declared.line, Some(9));
    // A group's ordinary rows are passed down, never judged on it.
    assert_eq!(group.applicability, Applicability::Checks);
    assert_eq!(names(group.judged()), ["tests-full"]);
}

#[test]
fn an_entry_of_the_same_name_overrides_an_inherited_row() {
    let tree = project();
    let set = set_of("override", &tree, &[]);
    assert_eq!(
        names(&set.rows),
        ["format", "tests", "lint"],
        "position is kept"
    );
    let tests = row(&set.rows, "tests");
    assert_eq!(tests.decl.command, Some(vec!["own-tests".to_string()]));
    assert_eq!(tests.declared.file, "tasks/override/task.toml");
    assert_eq!(
        tests.source,
        Source::Overrides {
            from: Box::new(Source::Inherited {
                from: "g".to_string()
            })
        }
    );
    assert_eq!(
        row(&set.rows, "format").source,
        Source::Inherited {
            from: "g".to_string()
        }
    );
}

#[test]
fn drop_removes_a_row_and_keeps_it_for_the_section() {
    let tree = project();
    let set = set_of("drops", &tree, &[]);
    assert_eq!(names(&set.rows), ["format", "tests"]);
    assert_eq!(set.dropped.len(), 1);
    assert_eq!(set.dropped[0].by, "drops");
    assert_eq!(set.dropped[0].row.decl.name, "lint");
    assert_eq!(
        set.dropped[0].row.source,
        Source::Inherited {
            from: "g".to_string()
        }
    );
    // Dropping a name that is not there is a warning, not an error.
    let findings: Vec<String> = set.findings.iter().map(ToString::to_string).collect();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("check nothing"), "{findings:?}");
    assert!(!set.findings[0].is_error());
}

#[test]
fn inherit_none_starts_with_nothing() {
    let tree = project();
    let set = set_of("alone", &tree, &[]);
    assert_eq!(set.inherit, Inherit::None);
    assert_eq!(names(&set.rows), ["only"]);
    assert_eq!(set.rows[0].source, Source::Own);
    assert_eq!(set_of("plain", &tree, &[]).inherit, Inherit::Parent);
}

#[test]
fn a_set_row_can_override_an_inherited_row_and_an_entry_one_from_a_set() {
    let tree = tree(&[
        (
            "",
            "g",
            "other",
            "[[checks.check]]\nname = \"format\"\ncommand = [\"top-format\"]\n",
        ),
        (
            "tasks/a",
            "a",
            "pr-ship",
            "[checks]\nuse = [\"rust\"]\n\n[[checks.check]]\nname = \"tests\"\ncommand = [\"mine\"]\n",
        ),
    ]);
    let set = set_of("a", &tree, &[]);
    let format = row(&set.rows, "format");
    assert_eq!(format.declared.file, "checks/sets/rust.toml");
    assert_eq!(
        format.source,
        Source::Overrides {
            from: Box::new(Source::Inherited {
                from: "g".to_string()
            })
        }
    );
    // The entry replaces the row the set added.
    assert_eq!(
        row(&set.rows, "tests").source,
        Source::Overrides {
            from: Box::new(Source::Set {
                set: "rust".to_string()
            })
        }
    );
}

fn closing_tree() -> DefinitionTree {
    tree(&[
        (
            "",
            "g",
            "other",
            "[[checks.closing]]\nname = \"full\"\ncommand = [\"full\"]\n",
        ),
        ("tasks/x", "x", "pr-ship", ""),
        ("tasks/y", "y", "pr-ship", ""),
        ("tasks/z", "z", "pr-ship", ""),
    ])
}

fn carries(tree: &DefinitionTree, task: &str, done: &[&str]) -> bool {
    let set = set_of(task, tree, done);
    let carried = set.rows.iter().any(|row| row.decl.name == "full");
    if carried {
        assert_eq!(
            row(&set.rows, "full").source,
            Source::Closing {
                group: "g".to_string()
            }
        );
    }
    carried
}

#[test]
fn closing_checks_reach_exactly_the_last_open_child() {
    let tree = closing_tree();
    // Everything open, or two open: no child carries them.
    for done in [&[][..], &["x"][..], &["y"][..], &["z"][..]] {
        for task in ["x", "y", "z"] {
            assert!(!carries(&tree, task, done), "{task} with {done:?} done");
        }
    }
    // One open: that child, and no earlier child.
    assert!(carries(&tree, "z", &["x", "y"]));
    assert!(!carries(&tree, "x", &["x", "y"]));
    assert!(!carries(&tree, "y", &["x", "y"]));
    assert!(carries(&tree, "x", &["y", "z"]));
    assert!(carries(&tree, "y", &["x", "z"]));
    // Nobody left open: nobody carries.
    for task in ["x", "y", "z"] {
        assert!(!carries(&tree, task, &["x", "y", "z"]));
    }
}

#[test]
fn the_group_keeps_its_closing_set_whoever_carries_it() {
    let tree = closing_tree();
    for done in [&[][..], &["x", "y"][..]] {
        let group = set_of("g", &tree, done);
        assert_eq!(names(&group.closing), ["full"]);
        assert_eq!(group.closing[0].source, Source::Own);
        assert!(group.rows.is_empty());
    }
}

#[test]
fn a_carried_closing_check_makes_the_carrier_judged_on_it() {
    let tree = closing_tree();
    let carrier = set_of("z", &tree, &["x", "y"]);
    assert_eq!(carrier.applicability, Applicability::Checks);
    assert_eq!(names(carrier.judged()), ["full"]);
    assert_eq!(carrier.required().count(), 1);
    let other = set_of("z", &tree, &[]);
    assert_eq!(other.applicability, Applicability::NoChecks);
}

fn nested_tree() -> DefinitionTree {
    tree(&[
        (
            "",
            "t",
            "other",
            "[[checks.closing]]\nname = \"top-closing\"\ncommand = [\"t\"]\n",
        ),
        ("tasks/p", "p", "pr-ship", ""),
        (
            "tasks/q",
            "q",
            "other",
            "[[checks.closing]]\nname = \"q-closing\"\ncommand = [\"q\"]\n",
        ),
        ("tasks/q/tasks/q1", "q1", "pr-ship", ""),
        ("tasks/q/tasks/q2", "q2", "pr-ship", ""),
    ])
}

#[test]
fn closing_checks_pass_through_a_child_group_to_its_last_child() {
    let tree = nested_tree();
    let done = ["p", "q1"];
    // q is the last open child of t, so it carries t's closing check into its own closing set.
    let group = set_of("q", &tree, &done);
    assert_eq!(names(&group.closing), ["q-closing", "top-closing"]);
    assert_eq!(group.closing[0].source, Source::Own);
    assert_eq!(
        group.closing[1].source,
        Source::Closing {
            group: "t".to_string()
        }
    );
    // q2 is the last open child of q, and q is the last open child of t: both come down.
    let last = set_of("q2", &tree, &done);
    assert_eq!(names(&last.rows), ["q-closing", "top-closing"]);
    assert_eq!(
        row(&last.rows, "q-closing").source,
        Source::Closing {
            group: "q".to_string()
        }
    );
    assert_eq!(
        row(&last.rows, "top-closing").source,
        Source::Closing {
            group: "t".to_string()
        }
    );
    // q1 is done and no longer carries anything.
    assert!(set_of("q1", &tree, &done).rows.is_empty());
}

#[test]
fn a_group_with_another_open_sibling_does_not_pass_the_outer_closing_check_down() {
    let tree = nested_tree();
    let done = ["q1"];
    // p and q are both open: q does not carry t's closing check, and neither does q2.
    let group = set_of("q", &tree, &done);
    assert_eq!(names(&group.closing), ["q-closing"]);
    let last = set_of("q2", &tree, &done);
    assert_eq!(names(&last.rows), ["q-closing"]);
    assert!(set_of("p", &tree, &done).rows.is_empty());
}

#[test]
fn a_document_or_spike_child_never_carries_closing_checks() {
    let tree = tree(&[
        (
            "",
            "g",
            "other",
            "[[checks.closing]]\nname = \"full\"\ncommand = [\"full\"]\n",
        ),
        ("tasks/x", "x", "pr-ship", ""),
        ("tasks/d", "d", "document", ""),
        ("tasks/s", "s", "spike", ""),
    ]);
    // Only the document is open: nobody carries, and the group judges the closing set.
    for task in ["x", "d", "s"] {
        assert!(set_of(task, &tree, &["x", "s"]).rows.is_empty(), "{task}");
    }
    // The leaf is the one left open next to a finished document.
    assert_eq!(names(&set_of("x", &tree, &["d", "s"]).rows), ["full"]);
}

#[test]
fn document_and_spike_tasks_have_no_checks_that_apply() {
    let tree = tree(&[
        (
            "",
            "g",
            "other",
            "[[checks.check]]\nname = \"lint\"\ncommand = [\"lint\"]\n",
        ),
        ("tasks/doc", "doc", "document", ""),
        (
            "tasks/spike",
            "spike",
            "spike",
            "[[checks.check]]\nname = \"own\"\ncommand = [\"x\"]\n",
        ),
        ("tasks/code", "code", "pr-merge", ""),
    ]);
    for task in ["doc", "spike"] {
        let set = set_of(task, &tree, &[]);
        assert_eq!(set.applicability, Applicability::NoChecksApply, "{task}");
        assert!(set.rows.is_empty() && set.closing.is_empty(), "{task}");
        assert!(set.judged().is_empty());
        assert_eq!(set.required().count(), 0);
    }
    assert_eq!(
        set_of("code", &tree, &[]).applicability,
        Applicability::Checks
    );
}

#[test]
fn a_task_with_no_rows_has_no_checks() {
    let tree = tree(&[
        ("", "g", "other", ""),
        ("tasks/a", "a", "pr-ship", ""),
        ("tasks/b", "b", "pr-ship", "[checks]\ninherit = \"none\"\n"),
    ]);
    for task in ["a", "b", "g"] {
        let set = set_of(task, &tree, &[]);
        assert_eq!(set.applicability, Applicability::NoChecks, "{task}");
        assert!(set.rows.is_empty() && set.closing.is_empty() && set.dropped.is_empty());
        assert!(set.findings.is_empty());
    }
}

#[test]
fn a_groups_ordinary_rows_are_not_judged_on_it() {
    let tree = tree(&[
        (
            "",
            "g",
            "other",
            "[[checks.check]]\nname = \"lint\"\ncommand = [\"lint\"]\n",
        ),
        ("tasks/a", "a", "pr-ship", ""),
    ]);
    let group = set_of("g", &tree, &[]);
    assert_eq!(names(&group.rows), ["lint"]);
    assert!(group.judged().is_empty());
    assert_eq!(group.applicability, Applicability::NoChecks);
    assert_eq!(names(&set_of("a", &tree, &[]).rows), ["lint"]);
}

#[test]
fn an_advisory_row_is_not_required() {
    let tree = tree(&[
        ("", "g", "other", ""),
        (
            "tasks/leaf",
            "leaf",
            "pr-ship",
            "[[checks.check]]\nname = \"a\"\ncommand = [\"x\"]\n\n[[checks.check]]\nname = \"b\"\ncommand = [\"x\"]\nrequired = false\n",
        ),
    ]);
    let set = set_of("leaf", &tree, &[]);
    assert_eq!(names(set.judged()), ["a", "b"]);
    let required: Vec<&str> = set.required().map(|row| row.decl.name.as_str()).collect();
    assert_eq!(required, ["a"]);
}

#[test]
fn problems_are_reported_with_file_and_check() {
    let tree = tree(&[
        ("", "g", "other", TOP),
        (
            "tasks/a",
            "a",
            "pr-ship",
            "[checks]\nuse = [\"nope\"]\n\n[[checks.check]]\nname = \"t\"\ncommand = [\"x\"]\nor_accepted = \"ghost\"\n\n[[checks.check]]\nname = \"u\"\ncommand = [\"x\"]\nor_accepted = \"u\"\n\n[[checks.check]]\nname = \"v\"\ncommand = [\"x\"]\nor_accepted = \"t\"\n\n[[checks.check]]\ncommand = [\"x\"]\n",
        ),
    ]);
    let set = set_of("a", &tree, &[]);
    let findings: Vec<String> = set.findings.iter().map(ToString::to_string).collect();
    let has = |needle: &str| findings.iter().any(|text| text.contains(needle));
    assert!(has("use names the set nope"), "{findings:?}");
    assert!(
        has("tasks/a/task.toml, line unknown, check t: or_accepted names ghost"),
        "{findings:?}"
    );
    assert!(
        has("tasks/a/task.toml, line unknown, check u: or_accepted names the check itself"),
        "{findings:?}"
    );
    assert!(
        !has("check v"),
        "an or_accepted that names another row is fine: {findings:?}"
    );
    assert!(has("(unnamed)"), "{findings:?}");
    assert_eq!(names(&set.rows), ["t", "u", "v"]);
}

#[test]
fn an_unknown_task_has_nothing() {
    let tree = project();
    let set = set_of("missing", &tree, &[]);
    assert_eq!(set.applicability, Applicability::UnknownTask);
    assert!(set.rows.is_empty());
    let empty = effective_set("g", &DefinitionTree::default(), &named(), &|_| false);
    assert_eq!(empty.applicability, Applicability::UnknownTask);
}

#[test]
fn named_sets_are_named_by_file_and_report_broken_files() {
    let sets = NamedSets::parse([
        ("checks/sets/rust.toml", RUST_SET),
        ("checks/sets/broken.toml", "[[check]]\nname = \n"),
        (
            "other/rust.toml",
            "[[check]]\nname = \"x\"\ncommand = [\"x\"]\n",
        ),
    ]);
    assert_eq!(sets.names().collect::<Vec<_>>(), ["rust"]);
    assert_eq!(sets.findings.len(), 2, "{:?}", sets.findings);
    assert_eq!(sets.findings[0].file, "checks/sets/broken.toml");
    assert_eq!(sets.findings[0].line, Some(2));
    assert!(sets.findings[1].message.contains("also defined by"));
    assert_eq!(sets.get("rust").unwrap().closing[0].name, "tests-full");
}
