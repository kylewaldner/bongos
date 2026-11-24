//! Scenarios of the workflow sandbox as a person meets them, each through the parts that decide it:
//! no OS sandbox under the default mode asks once per run and the grant lives in memory; an
//! unattended run is refused and never retried; a `wait` sees only the variables it names; the same
//! bad result from an agent and from a script reads the same; and a change to the project's
//! `runs.unsandboxed` is held for a person. The helpers are those of the neighbouring test modules:
//! the scripted runner whose nodes find no sandbox, the fake Claude, the sandbox probe that finds
//! none, and the approval hook's own fixtures. Every wait uses a fake clock.

use std::collections::BTreeSet;

use bongos_project_repo::ApprovalOpinion;
use bongos_workflow_model::scheduler::is_retried;
use bongos_workflow_model::unsandboxed::{self, Mode, REFUSED_TEXT};
use bongos_workflow_model::{FailureReason, UnattendedChoice, UnattendedSource};

use crate::WorkflowsSettings;
use crate::builtin_wait_tests::{
    DUMP_ENV, assert_satisfied, names, no_sandbox, path_var, person_grant as wait_grant, place,
    read_env, scratch, start_wait, vars, wait_node,
};
use crate::consent::tests::{
    World, asks, failures, person_grant, pending_keys, request, settle, started, state,
};
use crate::lifecycle::{BY_PERSON, RunStatus, StartRequest};
use crate::program_tests::{self, Fixture};
use crate::runner_claude_tests::{definition, failure, key, run_one};
use crate::runner_program::{DenialLog, Exit, ResultFile};
use crate::runtime::fake::NO_SANDBOX_REASON;
use crate::sandbox_switch::tests::{PROJECT, REFUSING, judge, needs};

#[test]
fn no_sandbox_asks_once_per_run_and_the_grant_is_only_in_memory() {
    // Ask is the default.
    assert_eq!(WorkflowsSettings::default().programs.unsandboxed, Mode::Ask);

    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let first = world.start(&mut lifecycle, request(1, "one"));
    settle(&mut lifecycle);
    assert_eq!(asks(&lifecycle, &first), 1);
    assert_eq!(lifecycle.status(&first), Some(RunStatus::WaitingOnYou));
    assert_eq!(
        lifecycle.consent_question(&first),
        Some(unsandboxed::ask_text("lint", NO_SANDBOX_REASON))
    );
    assert!(started(&lifecycle, &first).is_empty(), "nothing ran");

    // Allow runs the program, with the person's word as its permit.
    lifecycle
        .answer_unsandboxed(&first, true, BY_PERSON)
        .unwrap();
    settle(&mut lifecycle);
    assert_eq!(started(&lifecycle, &first), vec!["lint"]);
    let permits = &lifecycle.runtime().runner().permits;
    assert_eq!(permits.last().map(|(_, permit)| permit), Some(&person_grant()));

    // A second run does not borrow the first's answer: it asks for itself.
    let second = world.start(&mut lifecycle, request(2, "one"));
    settle(&mut lifecycle);
    assert_eq!(asks(&lifecycle, &second), 1);
    assert_eq!(
        pending_keys(&lifecycle, &second),
        Some(vec!["lint".to_string()])
    );
    assert!(started(&lifecycle, &second).is_empty(), "nothing ran");
    assert_eq!(lifecycle.consent_grant(&second), None);

    // The answer is in the run's record, the grant is not: a new process asks again.
    drop(lifecycle);
    let mut fresh = world.lifecycle(&["lint"]);
    fresh.load_runs(&world.tree).unwrap();
    assert_eq!(state(&fresh, &first).consent_history.len(), 1);
    assert_eq!(fresh.consent_grant(&first), None, "memory only");
}

#[test]
fn an_unattended_run_is_refused_with_the_label_and_not_retried() {
    let world = World::new();
    let mut lifecycle = world.lifecycle(&["lint"]);
    let start = StartRequest {
        unattended: Some(UnattendedChoice {
            value: true,
            source: UnattendedSource::Start,
        }),
        ..request(1, "one")
    };
    let run = world.start(&mut lifecycle, start);
    settle(&mut lifecycle);

    assert_eq!(asks(&lifecycle, &run), 0, "no person to ask");
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    assert!(started(&lifecycle, &run).is_empty(), "nothing ran");
    let failed = failures(&lifecycle, &run);
    assert_eq!(failed.len(), 1, "one failure, no second try: {failed:?}");
    assert_eq!(failed[0].1, FailureReason::SandboxRefused);
    assert_eq!(failed[0].1.label(), "refused: no OS sandbox");
    assert!(failed[0].2.starts_with(REFUSED_TEXT), "{}", failed[0].2);
    assert!(!is_retried(FailureReason::SandboxRefused));
    // The one start attempt carried a refusing permit.
    let permits = &lifecycle.runtime().runner().permits;
    assert_eq!(permits.len(), 1, "{permits:?}");
}

#[test]
fn a_wait_sees_only_the_variables_it_names() {
    let place = place(None);
    let path = path_var();
    let node = wait_node(
        &place,
        "gh",
        no_sandbox(),
        vars(&[
            ("PATH", &path),
            ("HOME", "/home/nobody"),
            ("LANG", "C.UTF-8"),
            ("NAMED", "kept"),
            ("SECRET", "not named"),
            ("GH_TOKEN", "a gh condition's only"),
            ("ANTHROPIC_API_KEY", "not named either"),
        ]),
    );
    let with =
        format!(r#"{{ condition = "command", {DUMP_ENV}, env = ["NAMED"], give_up_minutes = 5 }}"#);
    // A person's word lets it run without the sandbox, with the same cleared environment.
    start_wait(&node, &place, &with, "", wait_grant(), Mode::Ask).unwrap();
    assert_satisfied(&node);
    let env = read_env(&scratch(&place).join("env"));
    assert_eq!(
        names(&env),
        BTreeSet::from(["HOME", "LANG", "NAMED", "PATH", "TMPDIR"])
    );
    assert_eq!(env["NAMED"], "kept");
}

#[test]
fn the_same_bad_result_from_claude_and_from_a_script_reads_the_same() {
    // The fake Claude returns a result whose `mode` is not one of the choices.
    let (finish, _root) = run_one("invalid", |_| {});
    let from_claude = failure(&finish);
    assert_eq!(from_claude.reason, FailureReason::NodeOutputInvalid);

    // A script writes the same value for a node with the same output.
    let fixture = program_tests::without_python(program_tests::no_sandbox());
    let mut node = Fixture::node(&fixture, "");
    node.schema = definition().node("triage").unwrap().schema.clone();
    let from_script = crate::runner_program::classify(
        &key("triage"),
        &node,
        Exit::Code(0),
        false,
        &DenialLog::ended(Vec::new()),
        ResultFile::Text(r#"{"mode":"maybe","why":"x"}"#.to_string()),
        "",
    )
    .unwrap_err();
    assert_eq!(from_script.reason, FailureReason::ProgramResultInvalid);

    // One message: the node kind only changes the reason that heads it.
    assert!(from_claude.detail.contains("/mode"), "{}", from_claude.detail);
    assert_eq!(from_claude.detail, from_script.detail);
}

#[test]
fn a_change_to_the_project_runs_unsandboxed_key_is_held_for_a_person() {
    let held = needs("changes the workflow sandbox switch runs.unsandboxed");
    // Adding, removing or altering the key: all held, whoever wrote the change.
    let allowing = "title = \"Shop\"\n\n[runs]\nunsandboxed = \"allow\"\n";
    for (before, after) in [(PROJECT, REFUSING), (REFUSING, PROJECT), (REFUSING, allowing)] {
        assert_eq!(
            judge(&[("project.toml", before)], &[("project.toml", after)]),
            held,
            "{before} -> {after}"
        );
    }
    // A change that leaves the key alone is not held.
    let retitled = REFUSING.replace("Shop", "Store");
    assert_eq!(
        judge(
            &[("project.toml", REFUSING)],
            &[("project.toml", &retitled)]
        ),
        ApprovalOpinion::NoOpinion
    );
}
