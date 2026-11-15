use crate::decl::Inherit;
use crate::effective::{
    Applicability, Declared, Dropped, EffectiveSet, LooseningKind as K, Row, Source, loosening,
};
use crate::evidence_tests::row;

fn set(rows: Vec<Row>) -> EffectiveSet {
    EffectiveSet {
        task_id: "t1".to_string(),
        applicability: Applicability::Checks,
        is_group: false,
        inherit: Inherit::Parent,
        rows,
        closing: Vec::new(),
        dropped: Vec::new(),
        findings: Vec::new(),
    }
}

fn kinds(old: &str, new: &str) -> Vec<K> {
    loosening(&set(vec![row(old)]), &set(vec![row(new)]))
        .into_iter()
        .map(|l| l.kind)
        .collect()
}

#[test]
fn an_unchanged_or_tightened_check_loosens_nothing() {
    assert!(kinds("", "").is_empty());
    assert!(kinds("pass = [0, 2]\n", "pass = [0]\n").is_empty());
    assert!(kinds("", "fail = [1, 2]\n").is_empty());
    assert!(kinds("required = false\n", "").is_empty());
    assert!(kinds("", "summary = \"ok\"\nresult_file = true\n").is_empty());
}

#[test]
fn a_removed_row_is_found() {
    let found = loosening(&set(vec![row("")]), &set(Vec::new()));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].check, "tests");
    assert_eq!(found[0].kind, K::Removed);
}

#[test]
fn a_dropped_row_is_found_as_dropped() {
    let old = set(vec![row("")]);
    let mut new = set(Vec::new());
    new.dropped.push(Dropped {
        by: "t1".to_string(),
        row: row(""),
    });
    let found = loosening(&old, &new);
    assert_eq!(found[0].kind, K::Dropped);
}

#[test]
fn a_task_that_no_longer_has_checks_apply_loses_every_row() {
    let old = set(vec![row("")]);
    let mut new = set(vec![row("")]);
    new.applicability = Applicability::NoChecksApply;
    assert_eq!(loosening(&old, &new)[0].kind, K::Removed);
}

#[test]
fn made_advisory_is_found() {
    assert_eq!(kinds("", "required = false\n"), [K::MadeAdvisory]);
}

#[test]
fn wider_pass_codes_are_found_with_a_readable_message() {
    let found = loosening(&set(vec![row("")]), &set(vec![row("pass = [0, 2]\n")]));
    assert_eq!(
        found[0].kind,
        K::WiderPass {
            old: vec![0],
            new: vec![0, 2]
        }
    );
    assert_eq!(found[0].to_string(), "check tests: pass codes 0 -> 0, 2");
}

#[test]
fn a_narrower_fail_list_is_found() {
    assert_eq!(
        kinds("fail = [1, 2]\n", "fail = [1]\n"),
        [K::NarrowerFail {
            old: vec![1, 2],
            new: vec![1]
        }]
    );
    assert_eq!(kinds("", "fail = [2]\n").len(), 1);
}

#[test]
fn another_run_or_command_is_found() {
    assert_eq!(
        kinds("", "command = [\"true\"]\n"),
        [K::ChangedRun {
            old: "t".into(),
            new: "true".into()
        }]
    );
    assert_eq!(kinds("run = \"a\"\n", "run = \"b\"\n").len(), 1);
    assert!(kinds("run = \"a\"\n", "run = \"a\"\n").is_empty());
}

#[test]
fn a_removed_summary_or_result_file_is_found() {
    assert_eq!(kinds("summary = \"ok\"\n", ""), [K::RemovedSummary]);
    assert_eq!(kinds("result_file = true\n", ""), [K::RemovedResultFile]);
}

#[test]
fn a_scope_widened_from_all_to_a_script_is_found() {
    let found = kinds("scope = \"all\"\n", "scope = { script = \"s\" }\n");
    assert_eq!(
        found,
        [K::ScopeWidened {
            old: "all".into(),
            new: "script s".into()
        }]
    );
    assert_eq!(kinds("", "scope = { script = \"s\" }\n").len(), 1);
    assert_eq!(
        kinds(
            "scope = { script = \"s\" }\n",
            "scope = { script = \"s\" }\n"
        ),
        []
    );
    assert_eq!(
        kinds(
            "scope = { script = \"s\" }\n",
            "scope = { script = \"u\" }\n"
        )
        .len(),
        1
    );
    assert!(kinds("scope = { script = \"s\" }\n", "scope = \"all\"\n").is_empty());
}

#[test]
fn a_new_or_accepted_is_found() {
    assert_eq!(
        kinds("", "or_accepted = \"flake\"\n"),
        [K::NewAccepted { by: "flake".into() }]
    );
    assert!(kinds("or_accepted = \"flake\"\n", "or_accepted = \"flake\"\n").is_empty());
}

#[test]
fn several_findings_come_together_and_advisory_old_rows_are_ignored() {
    assert_eq!(kinds("", "required = false\npass = [0, 5]\n").len(), 2);
    let old = set(vec![row("required = false\n")]);
    assert!(loosening(&old, &set(Vec::new())).is_empty());
}

#[test]
fn a_group_is_compared_on_its_closing_set() {
    let mut old = set(Vec::new());
    old.is_group = true;
    old.closing = vec![Row {
        source: Source::Closing { group: "t1".into() },
        declared: Declared {
            file: "f".into(),
            check: "tests".into(),
            line: None,
        },
        ..row("")
    }];
    let mut new = old.clone();
    new.closing.clear();
    assert_eq!(loosening(&old, &new)[0].kind, K::Removed);
}
