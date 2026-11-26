use std::collections::{BTreeMap, BTreeSet};

use crate::references::{ReferenceInputs, WorkflowText, reference_problems};

const POOLS: &str = r#"schema = 1
[[pool]]
name = "jobs"
kind = "rw"

[[pool]]
name = "compile"
kind = "slots"
size = 2
"#;

const QUEUES: &str = r#"schema = 1
[[queue]]
name = "fixes"
item = { test = { type = "text" }, task = { type = "task", optional = true } }
task_field = "task"

[[queue]]
name = "tasked"
item = { test = { type = "text" }, task = { type = "task" } }
task_field = "task"

[[queue]]
name = "reports"
item = { text = { type = "text" } }
"#;

const READS_GOAL: &str = r#"schema = 1
name = "fix release"
start = "work"
[inputs]
goal = { type = "text", from = "task.goal" }
failing = { type = "text", from = "literal:none" }
[nodes.work]
kind = "agent"
prompt = "work.md"
"#;

const MERGES: &str = r#"schema = 1
name = "fix release"
start = "work"
[inputs]
failing = { type = "text", from = "literal:none" }
[nodes.work]
kind = "agent"
prompt = "work.md"
[nodes.land]
kind = "program"
use = "merge"
"#;

const PLAIN: &str = r#"schema = 1
name = "triage report"
start = "work"
[inputs]
report = { type = "text", from = "literal:none" }
[nodes.work]
kind = "agent"
prompt = "work.md"
"#;

fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|line| line.contains(needle))
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
}

fn workflow(folder: &str, text: &str) -> WorkflowText {
    WorkflowText {
        label: format!("workflows/{folder}"),
        text: text.to_string(),
    }
}

fn base() -> ReferenceInputs {
    ReferenceInputs {
        pools: POOLS.to_string(),
        queues: QUEUES.to_string(),
        other_workflows: Some(BTreeMap::new()),
        task_ids: Some(BTreeSet::from(["0193-task".to_string()])),
        ..ReferenceInputs::default()
    }
}

fn lines(inputs: &ReferenceInputs) -> Vec<String> {
    reference_problems(inputs)
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn lane_table(kind_body: &str) -> String {
    format!("schema = 1\n\n[[kind]]\nname = \"build\"\n{kind_body}")
}

#[test]
fn nothing_declared_and_nothing_named_is_no_problem() {
    assert!(reference_problems(&ReferenceInputs::default()).is_empty());
    let empty_files = ReferenceInputs {
        pools: "\n".to_string(),
        queues: "  ".to_string(),
        triggers: String::new(),
        lanes: String::new(),
        ..ReferenceInputs::default()
    };
    assert!(reference_problems(&empty_files).is_empty());
}

#[test]
fn an_undeclared_pool_in_a_lane_binding_is_refused_by_line() {
    let lanes = lane_table("[kind.binding]\npools = [\"jobs\",\n  \"jobz\"]\n");
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        ..base()
    };
    let line = line_of(&lanes, "\"jobz\"");
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {line}: pool `jobz` is not declared in pools/pools.toml"
        )]
    );
}

#[test]
fn an_undeclared_pool_in_a_hook_is_refused() {
    let lanes = lane_table(
        "[kind.binding.hooks]\nbefore_job = { run = \"lanes/hooks/preflight\", needs = [\"disk-io\"] }\n",
    );
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        ..base()
    };
    let line = line_of(&lanes, "before_job");
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {line}: pool `disk-io` is not declared in pools/pools.toml"
        )]
    );
}

#[test]
fn a_pool_removed_while_a_lane_names_it_is_refused() {
    let lanes = lane_table("[kind.binding]\npools = [\"compile\"]\n");
    let with_pool = ReferenceInputs {
        lanes: lanes.clone(),
        ..base()
    };
    assert!(lines(&with_pool).is_empty());
    let removed = ReferenceInputs {
        pools: "schema = 1\n[[pool]]\nname = \"jobs\"\nkind = \"rw\"\n".to_string(),
        ..with_pool
    };
    assert_eq!(
        lines(&removed),
        [format!(
            "lanes/lanes.toml line {}: pool `compile` is not declared in pools/pools.toml",
            line_of(&lanes, "compile")
        )]
    );
}

#[test]
fn a_need_the_pool_cannot_give_names_file_and_line() {
    let lanes = lane_table("[kind.binding]\npools = [{ pool = \"compile\", count = 3 }]\n");
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {}: count 3 is above the size 2 of pool `compile`",
            line_of(&lanes, "count = 3")
        )]
    );
}

#[test]
fn an_undeclared_pool_in_a_workflow_node_needs_is_refused_by_line() {
    let text = "schema = 1\nname = \"build\"\nstart = \"work\"\n[nodes.work]\nkind = \"agent\"\nprompt = \"work.md\"\nneeds = [\"jobs\", { pool = \"gpu\" }]\n";
    let inputs = ReferenceInputs {
        project_workflows: vec![workflow("build", text)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "workflows/build line {}: pool `gpu` is not declared in pools/pools.toml",
            line_of(text, "needs")
        )]
    );
}

#[test]
fn an_undeclared_pool_in_a_check_needs_is_refused_by_line() {
    let task = "id = \"0193-task\"\ntitle = \"t\"\n\n[[checks.check]]\nname = \"unit\"\ncommand = [\"true\"]\nneeds = [\"jobs\"]\n\n[[checks.check]]\nname = \"slow\"\ncommand = [\"true\"]\nneeds = [{ pool = \"compile\", count = 1 }, \"jobz\"]\n";
    let set = "[[check]]\nname = \"lint\"\ncommand = [\"true\"]\nneeds = [{ pool = \"gone\", mode = \"shared\" }]\n";
    let inputs = ReferenceInputs {
        check_files: vec![
            ("checks/sets/style.toml".to_string(), set.to_string()),
            ("tasks/a/task.toml".to_string(), task.to_string()),
        ],
        ..base()
    };
    let slow = task
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("[[checks.check]]"))
        .nth(1)
        .map(|(index, _)| index + 1)
        .unwrap();
    assert_eq!(
        lines(&inputs),
        [
            "checks/sets/style.toml line 1: pool `gone` is not declared in pools/pools.toml"
                .to_string(),
            format!(
                "tasks/a/task.toml line {slow}: pool `jobz` is not declared in pools/pools.toml"
            ),
        ]
    );
}

#[test]
fn a_pulls_entry_on_an_unknown_queue_is_refused() {
    let lanes = lane_table(
        "pulls = [{ queue = \"fixez\", workflow = \"triage report\" }]\n[kind.binding]\ntrees = [{ repo = \"app\" }]\n",
    );
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        project_workflows: vec![workflow("triage-report", PLAIN)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {}: queue `fixez` is not declared in queues/queues.toml",
            line_of(&lanes, "pulls")
        )]
    );
}

#[test]
fn a_pulls_workflow_reading_task_goal_is_refused_naming_the_input() {
    let lanes =
        lane_table("pulls = [{ queue = \"fixes\", workflow = \"fix release\", repo = \"app\" }]\n");
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        project_workflows: vec![workflow("fix-release", READS_GOAL)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "workflows/fix-release line {}: input goal reads task.goal; a run with no task cannot use it; lanes/lanes.toml line {} runs it for items of queue `fixes`, which may have no task",
            line_of(READS_GOAL, "goal ="),
            line_of(&lanes, "pulls")
        )]
    );
}

#[test]
fn a_task_input_the_pulls_entry_sets_itself_is_not_read_from_the_task() {
    let lanes = lane_table(
        "pulls = [{ queue = \"fixes\", workflow = \"fix release\", repo = \"app\", inputs = { goal = \"item.test\" } }]\n",
    );
    let inputs = ReferenceInputs {
        lanes,
        project_workflows: vec![workflow("fix-release", READS_GOAL)],
        ..base()
    };
    assert!(lines(&inputs).is_empty(), "{:?}", lines(&inputs));
}

#[test]
fn a_pulls_input_naming_a_missing_item_field_is_refused() {
    let lanes = lane_table(
        "pulls = [{ queue = \"fixes\", workflow = \"triage report\", repo = \"app\", inputs = { report = \"item.tst\" } }]\n",
    );
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        project_workflows: vec![workflow("triage-report", PLAIN)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {}: input `report` reads item.tst, which is not an item field of queue `fixes`",
            line_of(&lanes, "pulls")
        )]
    );
}

#[test]
fn a_merge_node_in_a_pulls_workflow_needs_merge_into_naming_both_lines() {
    let lanes =
        lane_table("pulls = [{ queue = \"fixes\", workflow = \"fix release\", repo = \"app\" }]\n");
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        project_workflows: vec![workflow("fix-release", MERGES)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "workflows/fix-release line {}: a merge node needs merge_into on lanes/lanes.toml line {}, because an item without a task has no task branch",
            line_of(MERGES, "use = \"merge\""),
            line_of(&lanes, "pulls")
        )]
    );

    let with_merge_into = ReferenceInputs {
        lanes: lane_table(
            "pulls = [{ queue = \"fixes\", workflow = \"fix release\", repo = \"app\", merge_into = \"queue/fixes\" }]\n",
        ),
        ..inputs
    };
    assert!(lines(&with_merge_into).is_empty());
}

#[test]
fn the_same_workflows_pulled_from_a_queue_whose_items_have_a_task_pass() {
    let lanes = lane_table("pulls = [{ queue = \"tasked\", workflow = \"fix release\" }]\n");
    for text in [MERGES, READS_GOAL] {
        let inputs = ReferenceInputs {
            lanes: lanes.clone(),
            project_workflows: vec![workflow("fix-release", text)],
            ..base()
        };
        assert!(lines(&inputs).is_empty(), "{:?}", lines(&inputs));
    }
}

#[test]
fn a_pulls_entry_on_a_kind_with_no_trees_needs_repo() {
    let lanes = "schema = 1\n\n[[kind]]\nname = \"docs\"\npulls = [{ queue = \"reports\", workflow = \"triage report\" }]\n";
    let inputs = ReferenceInputs {
        lanes: lanes.to_string(),
        project_workflows: vec![workflow("triage-report", PLAIN)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {}: kind docs has no lane tree; this pulls entry needs repo",
            line_of(lanes, "pulls")
        )]
    );

    let with_trees = ReferenceInputs {
        lanes: format!("{lanes}[kind.binding]\ntrees = [{{ repo = \"app\" }}]\n"),
        ..inputs.clone()
    };
    assert!(lines(&with_trees).is_empty(), "{:?}", lines(&with_trees));

    let tasked = ReferenceInputs {
        lanes: lanes.replace("reports", "tasked"),
        ..inputs
    };
    assert!(lines(&tasked).is_empty(), "{:?}", lines(&tasked));
}

#[test]
fn a_pulls_workflow_that_does_not_resolve_is_refused() {
    let lanes = lane_table("pulls = [{ queue = \"tasked\", workflow = \"no such\" }]\n");
    let inputs = ReferenceInputs {
        lanes: lanes.clone(),
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "lanes/lanes.toml line {}: workflow `no such` does not resolve in the project, user or shipped workflows",
            line_of(&lanes, "pulls")
        )]
    );

    let user_layer = ReferenceInputs {
        other_workflows: Some(BTreeMap::from([(
            "no such".to_string(),
            WorkflowText {
                label: "/user/workflows/no-such".to_string(),
                text: PLAIN.replace("triage report", "no such"),
            },
        )])),
        ..inputs.clone()
    };
    assert!(lines(&user_layer).is_empty());

    let layers_unknown = ReferenceInputs {
        other_workflows: None,
        ..inputs
    };
    assert!(lines(&layers_unknown).is_empty());
}

const TRIGGER_REPO: &str = r#"schema = 1
[[trigger]]
name = "release-build"
on = { all_done = { work_kind = "feature" } }
action = { workflow = "fix release", repo = "app", base = "main" }
"#;

#[test]
fn a_repo_trigger_naming_a_workflow_reading_task_goal_is_refused_naming_the_input() {
    let inputs = ReferenceInputs {
        triggers: TRIGGER_REPO.to_string(),
        project_workflows: vec![workflow("fix-release", READS_GOAL)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "workflows/fix-release line {}: input goal reads task.goal; a run with no task cannot use it; trigger `release-build` on triggers/triggers.toml line {} runs it with no task",
            line_of(READS_GOAL, "goal ="),
            line_of(TRIGGER_REPO, "action")
        )]
    );
}

#[test]
fn a_merge_node_in_a_trigger_workflow_with_neither_task_nor_repo_is_refused() {
    let triggers = "schema = 1\n[[trigger]]\nname = \"triage\"\non = { queue = \"reports\", consume = true }\naction = { workflow = \"fix release\", merge_into = \"release\" }\n";
    let inputs = ReferenceInputs {
        triggers: triggers.to_string(),
        project_workflows: vec![workflow("fix-release", MERGES)],
        ..base()
    };
    let action = line_of(triggers, "action");
    assert_eq!(
        lines(&inputs),
        [
            format!("triggers/triggers.toml line {action}: base and merge_into need repo"),
            format!(
                "workflows/fix-release line {}: a run with no task and no repo has no tree to merge",
                line_of(MERGES, "use = \"merge\"")
            ),
        ]
    );
}

#[test]
fn a_trigger_with_merge_into_but_no_repo_is_refused() {
    let triggers = "schema = 1\n[[trigger]]\nname = \"nightly\"\non = { timer = { at = [\"03:00\"] } }\ncatch_up = false\naction = { workflow = \"triage report\", merge_into = \"release\" }\n";
    let inputs = ReferenceInputs {
        triggers: triggers.to_string(),
        project_workflows: vec![workflow("triage-report", PLAIN)],
        ..base()
    };
    assert_eq!(
        lines(&inputs),
        [format!(
            "triggers/triggers.toml line {}: base and merge_into need repo",
            line_of(triggers, "action")
        )]
    );
}

#[test]
fn a_trigger_repo_run_merges_only_with_merge_into() {
    let triggers = TRIGGER_REPO.replace("base = \"main\"", "merge_into = \"release\"");
    let inputs = ReferenceInputs {
        triggers,
        project_workflows: vec![workflow("fix-release", MERGES)],
        ..base()
    };
    assert!(lines(&inputs).is_empty(), "{:?}", lines(&inputs));
    let without = ReferenceInputs {
        triggers: TRIGGER_REPO.to_string(),
        ..inputs
    };
    assert_eq!(
        lines(&without),
        [format!(
            "workflows/fix-release line {}: a merge node needs merge_into on triggers/triggers.toml line {}, because a run without a task has no task branch",
            line_of(MERGES, "use = \"merge\""),
            line_of(TRIGGER_REPO, "action")
        )]
    );
}

#[test]
fn trigger_task_ids_queues_and_item_fields_must_exist() {
    let triggers = r#"schema = 1
[[trigger]]
name = "after"
on = { task_done = "0193-gone" }
action = { workflow = "fix release", task = "0193-missing" }

[[trigger]]
name = "requeue"
on = { run_ended = { workflow = "fix release", result = "failed" } }
action = { push = { queue = "nowhere", fields = { test = "literal:x" } } }

[[trigger]]
name = "push-bad-field"
on = { all_done = { under = "0193-task" } }
action = { push = { queue = "fixes", fields = { tset = "literal:x" } } }

[[trigger]]
name = "triage"
on = { queue = "reports", consume = true }
action = { workflow = "triage report", repo = "app", inputs = { report = "item.txt" } }
"#;
    let inputs = ReferenceInputs {
        triggers: triggers.to_string(),
        project_workflows: vec![
            workflow("fix-release", READS_GOAL),
            workflow("triage-report", PLAIN),
        ],
        ..base()
    };
    let at = |needle: &str| line_of(triggers, needle);
    assert_eq!(
        lines(&inputs),
        [
            format!(
                "triggers/triggers.toml line {}: trigger `after`: task `0193-gone` is not in the definitions",
                at("task_done")
            ),
            format!(
                "triggers/triggers.toml line {}: trigger `after`: task `0193-missing` is not in the definitions",
                at("0193-missing")
            ),
            format!(
                "triggers/triggers.toml line {}: queue `nowhere` is not declared in queues/queues.toml",
                at("nowhere")
            ),
            format!(
                "triggers/triggers.toml line {}: trigger `push-bad-field`: push field `tset` is not an item field of queue `fixes`",
                at("tset")
            ),
            format!(
                "triggers/triggers.toml line {}: input `report` reads item.txt, which is not an item field of queue `reports`",
                at("item.txt")
            ),
        ]
    );
}

#[test]
fn a_fully_valid_set_passes() {
    let lanes = r#"schema = 1

[[kind]]
name = "build"
pulls = [
  { queue = "fixes", priority = "before_picker", workflow = "fix release", inputs = { failing = "item.test" }, unattended = true, repo = "app", base = "main", merge_into = "queue/fixes" },
]
[kind.binding]
pools = ["jobs"]
trees = [{ repo = "app" }]
[kind.binding.hooks]
before_job = { run = "lanes/hooks/preflight", needs = [{ pool = "compile", count = 2 }], timeout = "10m" }
"#;
    let triggers = r#"schema = 1
[[trigger]]
name = "nightly-check"
on = { timer = { at = ["03:00"] } }
action = { workflow = "fix release", task = "0193-task" }

[[trigger]]
name = "release-build"
on = { all_done = { work_kind = "feature" } }
action = { workflow = "fix release", repo = "app", base = "main", merge_into = "release" }

[[trigger]]
name = "triage"
on = { queue = "reports", consume = true }
action = { workflow = "triage report", inputs = { report = "item.text" } }

[[trigger]]
name = "requeue-to-fixes"
on = { run_ended = { workflow = "fix release", result = "failed" } }
action = { push = { queue = "fixes", fields = { test = "literal:see the release check run" } } }
"#;
    let node_needs = MERGES.replace(
        "prompt = \"work.md\"",
        "prompt = \"work.md\"\nneeds = [{ pool = \"jobs\", mode = \"exclusive\" }]",
    );
    let inputs = ReferenceInputs {
        lanes: lanes.to_string(),
        triggers: triggers.to_string(),
        project_workflows: vec![
            workflow("fix-release", &node_needs),
            workflow("triage-report", PLAIN),
        ],
        check_files: vec![(
            "task.toml".to_string(),
            "id = \"0193-task\"\ntitle = \"t\"\n[[checks.check]]\nname = \"unit\"\ncommand = [\"true\"]\nneeds = [\"jobs\"]\n"
                .to_string(),
        )],
        ..base()
    };
    assert!(lines(&inputs).is_empty(), "{:?}", lines(&inputs));
}
