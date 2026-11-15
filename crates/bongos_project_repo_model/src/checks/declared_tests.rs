use bongos_work_model::{ProjectFile, parse_project};

use crate::checks::{CheckPhase, landing_declaration_problems};
use crate::test_trees::PROJECT;

fn project(checks: &str) -> ProjectFile {
    parse_project(&format!("{PROJECT}{checks}")).unwrap()
}

fn reasons(checks: &str) -> Vec<String> {
    landing_declaration_problems(&project(checks), &["rebalance"])
        .into_iter()
        .map(|problem| {
            assert_eq!(problem.path, "project.toml");
            problem.reason
        })
        .collect()
}

#[test]
fn a_declaration_may_not_make_a_check_amending() {
    assert_eq!(
        reasons(
            "[[landing.checks]]\nname = \"plan-format\"\nphase = \"amending\"\nrun = { script = \"checks/plan-format\" }\n"
        ),
        [
            "landing check plan-format: phase \"amending\" is not allowed; only checks bongos ships can amend"
        ]
    );
}

#[test]
fn an_amending_check_may_only_be_disabled() {
    assert!(reasons("[[landing.checks]]\nname = \"rebalance\"\nenabled = false\n").is_empty());
    let refusal =
        "landing check rebalance is an amending check; it may only be named to set enabled = false";
    assert_eq!(
        reasons(
            "[[landing.checks]]\nname = \"rebalance\"\nphase = \"in-queue\"\nenabled = false\n"
        ),
        [refusal]
    );
    assert_eq!(
        reasons("[[landing.checks]]\nname = \"rebalance\"\n"),
        [refusal]
    );
}

#[test]
fn approval_no_state_and_schema_cannot_be_disabled() {
    for name in ["approval", "no-state", "schema"] {
        assert_eq!(
            reasons(&format!(
                "[[landing.checks]]\nname = \"{name}\"\nenabled = false\n"
            )),
            [format!("landing check {name} cannot be disabled")]
        );
    }
    assert!(reasons("[[landing.checks]]\nname = \"secrets\"\nenabled = false\n").is_empty());
}

#[test]
fn entries_are_checked_for_their_keys_phase_and_applies() {
    let valid = "[[landing.checks]]\nname = \"plan-format\"\napplies = { paths = [\"documents/**\"] }\nphase = \"before-queue\"\nrun = { script = \"checks/plan-format\" }\n";
    assert!(reasons(valid).is_empty());
    assert_eq!(
        reasons(
            "[[landing.checks]]\nname = \"x\"\nphase = \"soon\"\napplies = \"never\"\ncolor = 1\n"
        ),
        [
            "landing check x: unknown key color",
            "landing check x: phase must be \"before-queue\" or \"in-queue\"",
            "landing check x: applies must be \"always\", \"tasks\" or { paths = [...] }",
        ]
    );
    assert_eq!(CheckPhase::parse("in-queue"), Some(CheckPhase::InQueue));
    assert_eq!(CheckPhase::parse("amending"), None);
    assert_eq!(CheckPhase::BeforeQueue.word(), "before-queue");
}
