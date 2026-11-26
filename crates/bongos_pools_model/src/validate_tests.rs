use crate::validate::{
    Problem, definition_path_problem, is_valid_name, validate_pools, validate_queues,
    validate_triggers,
};

fn lines(problems: &[Problem]) -> Vec<usize> {
    problems.iter().map(|problem| problem.line).collect()
}

#[test]
fn valid_examples_have_no_problems() {
    let pools = "schema = 1\n[[pool]]\nname = \"a\"\nkind = \"slots\"\nsize = 1\nmax_hold = \"4h\"\n[[pool]]\nname = \"m\"\nkind = \"mutex\"\nscope = \"machine\"\n[[pool]]\nname = \"mem\"\nkind = \"guard\"\nscope = \"machine\"\nguard = { free_disk_gb = 5, path = \"/var\" }\n";
    assert_eq!(validate_pools("pools/pools.toml", pools), vec![]);
}

#[test]
fn a_missing_or_empty_file_declares_nothing() {
    assert_eq!(validate_pools("pools/pools.toml", ""), vec![]);
    assert_eq!(validate_queues("queues/queues.toml", "\n"), vec![]);
    assert_eq!(validate_triggers("triggers/triggers.toml", ""), vec![]);
}

#[test]
fn problems_name_file_and_line() {
    let text = "schema = 1\n[[pool]]\nname = \"a\"\nkind = \"rw\"\nsize = 2\n";
    let problems = validate_pools("pools/pools.toml", text);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].file, "pools/pools.toml");
    assert_eq!(problems[0].line, 5);
    assert!(
        problems[0]
            .to_string()
            .starts_with("pools/pools.toml line 5: ")
    );
}

#[test]
fn pool_kind_rules() {
    let text = "schema = 1
[[pool]]
name = \"s\"
kind = \"slots\"
[[pool]]
name = \"z\"
kind = \"slots\"
size = 0
[[pool]]
name = \"mu\"
kind = \"mutex\"
size = 1
[[pool]]
name = \"g\"
kind = \"guard\"
[[pool]]
name = \"r\"
kind = \"rw\"
guard = { free_memory_gb = 1 }
[[pool]]
name = \"h\"
kind = \"rw\"
max_hold = \"soon\"
";
    let problems = validate_pools("p.toml", text);
    assert_eq!(lines(&problems), vec![2, 8, 12, 13, 19, 23]);
}

#[test]
fn a_machine_pool_with_a_script_guard_is_refused() {
    let text = "schema = 1\n[[pool]]\nname = \"m\"\nkind = \"slots\"\nsize = 1\nscope = \"machine\"\nguard = { script = \"pools/scripts/x\" }\n[[pool]]\nname = \"n\"\nkind = \"guard\"\nscope = \"machine\"\nguard = { free_disk_gb = 5, path = \"{project_folder}\" }\n";
    let problems = validate_pools("p.toml", text);
    assert_eq!(lines(&problems), vec![7, 12]);
    assert!(problems[0].message.contains("script guard"));
    assert!(problems[1].message.contains("project_folder"));
    let project = text.replace("machine", "project");
    assert_eq!(validate_pools("p.toml", &project), vec![]);
}

#[test]
fn guard_script_paths_stay_inside_definitions() {
    let text =
        "schema = 1\n[[pool]]\nname = \"a\"\nkind = \"guard\"\nguard = { script = \"../out\" }\n";
    assert_eq!(lines(&validate_pools("p.toml", text)), vec![5]);
    assert!(definition_path_problem("/abs").is_some());
    assert!(definition_path_problem("pools/scripts/ok").is_none());
}

#[test]
fn names_are_checked_and_unique() {
    let text = "schema = 1\n[[pool]]\nname = \"a\"\nkind = \"rw\"\n[[pool]]\nname = \"a\"\nkind = \"rw\"\n[[pool]]\nname = \"Bad_name\"\nkind = \"rw\"\n";
    let problems = validate_pools("p.toml", text);
    assert_eq!(lines(&problems), vec![5, 8]);
    assert!(problems[0].message.contains("already declared on line 2"));
    assert!(is_valid_name("a-b2"));
    assert!(!is_valid_name("2a"));
    assert!(!is_valid_name(""));
}

#[test]
fn unknown_keys_and_syntax_errors_name_a_line() {
    let problems = validate_pools(
        "p.toml",
        "schema = 1\n[[pool]]\nname = \"a\"\nkind = \"rw\"\nbogus = 1\n",
    );
    assert_eq!(lines(&problems), vec![5]);
    let problems = validate_queues("q.toml", "schema = 1\n[[queue]\n");
    assert_eq!(lines(&problems), vec![2]);
    let problems = validate_triggers("t.toml", "schema = 3\n");
    assert_eq!(lines(&problems), vec![1]);
}

#[test]
fn queue_rules() {
    let text = "schema = 1
[[queue]]
name = \"a\"
item = { x = { type = \"text\" } }
task_field = \"x\"
[[queue]]
name = \"b\"
item = { x = { type = \"text\" } }
task_field = \"y\"
[[queue]]
name = \"c\"
item = { x = { type = \"file\" } }
[[queue]]
name = \"d\"
max_attempts = 0
on_exhausted = \"explode\"
";
    let problems = validate_queues("q.toml", text);
    assert_eq!(lines(&problems), vec![5, 9, 12, 15, 16]);
}

#[test]
fn trigger_rules() {
    let text = "schema = 1
[[trigger]]
name = \"a\"
on = { timer = { at = [\"25:00\"] } }
action = { dispatcher = {} }
[[trigger]]
name = \"b\"
on = { timer = { every = \"10s\" } }
action = { dispatcher = {} }
[[trigger]]
name = \"c\"
on = { timer = { at = [\"01:00\"], days = [\"someday\"] } }
action = { dispatcher = {} }
[[trigger]]
name = \"d\"
on = { task_done = \"x\", queue = \"q\" }
action = { dispatcher = {} }
[[trigger]]
name = \"e\"
on = { task_done = \"x\" }
action = { workflow = \"w\", base = \"main\" }
[[trigger]]
name = \"f\"
on = { task_done = \"x\" }
action = { push = { queue = \"q\" }, dispatcher = {} }
";
    let problems = validate_triggers("t.toml", text);
    assert_eq!(lines(&problems), vec![4, 8, 12, 16, 21, 25]);
    assert!(problems[0].message.contains("HH:MM"));
    assert!(problems[1].message.contains("below one minute"));
    assert!(problems[2].message.contains("weekday"));
    assert!(
        problems[4]
            .message
            .contains("base and merge_into need repo")
    );
}
