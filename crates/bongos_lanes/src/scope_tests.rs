//! Tests of the `lane-run-scope` approval hook.

use std::collections::BTreeMap;
use std::path::Path;

use bongos_project_repo::{ApprovalHook, ApprovalInput, ApprovalOpinion, AutomaticRule};
use bongos_project_repo_model::checks::{
    ApprovalPolicy, ApprovalVerdict, Judgment, LiveFacts, judge,
};
use bongos_project_repo_model::{Author, AuthorKind, ChangeMeta, ChangeSummary, FileChange};

use crate::runs::LaneRuns;
use crate::scope::LaneRunScope;
use crate::test_support::{lanes_state, tree};
use bongos_lanes_model::{LanesState, OwnerState, TaskId};

const PROJECT: &str = "project-1";

fn hook() -> LaneRunScope {
    hook_with(lanes_state())
}

fn hook_with(state: LanesState) -> LaneRunScope {
    let runs = LaneRuns::default();
    runs.set(PROJECT, state);
    LaneRunScope::new(runs)
}

fn workflow_run(run: &str) -> Author {
    let mut author = Author::workflow("build");
    author.run = Some(run.to_string());
    author
}

fn summary(files: &[(&str, FileChange)], redefined: &[&str]) -> ChangeSummary {
    ChangeSummary {
        files: files
            .iter()
            .map(|(path, change)| (path.to_string(), *change))
            .collect(),
        tasks_redefined: redefined.iter().map(|id| id.to_string()).collect(),
        ..ChangeSummary::default()
    }
}

/// The hook's opinion of `summary` by `author`, and check 9's verdict with only this hook.
fn opinion_and_verdict(
    author: Author,
    summary: &ChangeSummary,
) -> (ApprovalOpinion, ApprovalVerdict) {
    judged_by(&hook(), author, summary)
}

/// `hook`'s opinion of `summary` by `author`, and check 9's verdict with only this hook.
fn judged_by(
    hook: &LaneRunScope,
    author: Author,
    summary: &ChangeSummary,
) -> (ApprovalOpinion, ApprovalVerdict) {
    let master = tree(&["tasks/feature", "tasks/feature/tasks/leaf", "tasks/other"]);
    let meta = ChangeMeta::new("change", author);
    let live = LiveFacts::from_states(BTreeMap::new());
    let opinion = hook.judge(&ApprovalInput {
        change_id: "c1",
        project_id: PROJECT,
        summary,
        meta: &meta,
        live: &live,
        master: &master,
        change: &master,
        master_root: Path::new("master"),
        change_root: Path::new("change"),
    });
    let kind = meta.author.kind();
    let policy = ApprovalPolicy::default();
    // Check 9 consults a rule's hook only for the author kinds the policy gives that rule.
    let rule_opinions = if policy.rule_for(kind) == hook.rule() {
        vec![(hook.name().to_string(), opinion.clone())]
    } else {
        Vec::new()
    };
    let verdict = judge(&Judgment {
        policy: &policy,
        author_kind: kind,
        rows: &[],
        amend_patch_id: "",
        rule_opinions: &rule_opinions,
        hook_opinions: &[],
        rollback: None,
    });
    (opinion, verdict)
}

/// A change inside the owned task's `documents/` and `tasks/`.
fn inside() -> ChangeSummary {
    summary(
        &[
            ("tasks/feature/documents/design.md", FileChange::Added),
            ("tasks/feature/tasks/leaf/task.toml", FileChange::Modified),
            ("tasks/feature/tasks/leaf/plan.md", FileChange::Added),
        ],
        &["leaf"],
    )
}

#[test]
fn lanes_the_hook_is_the_declared_scope_rule() {
    let hook = hook();
    assert_eq!(hook.name(), "lane-run-scope");
    assert_eq!(hook.rule(), Some(AutomaticRule::DeclaredScope));
}

#[test]
fn lanes_a_lane_runs_change_inside_its_owned_documents_and_tasks_is_approved() {
    let (opinion, verdict) = opinion_and_verdict(workflow_run("run-lane"), &inside());
    assert!(
        matches!(opinion, ApprovalOpinion::Approve(_)),
        "{opinion:?}"
    );
    assert!(verdict.is_approved(), "{verdict:?}");
}

#[test]
fn lanes_the_same_change_by_a_hand_started_run_waits_for_a_person() {
    let author = workflow_run("run-by-hand");
    assert_eq!(author.kind(), AuthorKind::Workflow);
    let (opinion, verdict) = opinion_and_verdict(author, &inside());
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
    assert!(!verdict.is_approved(), "{verdict:?}");
    assert!(verdict.waiting_reason().is_some());

    // A run with no run id, and an agent, get no opinion either.
    let (opinion, _) = opinion_and_verdict(Author::workflow("build"), &inside());
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
    let mut agent = Author::agent("agent");
    agent.run = Some("run-lane".to_string());
    let (opinion, _) = opinion_and_verdict(agent, &inside());
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
}

#[test]
fn lanes_a_lane_runs_edit_of_its_top_level_task_toml_waits_for_a_person() {
    let own_file = summary(
        &[("tasks/feature/task.toml", FileChange::Modified)],
        &["feature"],
    );
    let (opinion, verdict) = opinion_and_verdict(workflow_run("run-lane"), &own_file);
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
    assert!(!verdict.is_approved(), "{verdict:?}");
    assert!(verdict.waiting_reason().is_some());

    // The file alone, without the summary naming the task, waits too.
    let file_only = summary(&[("tasks/feature/task.toml", FileChange::Modified)], &[]);
    let (opinion, verdict) = opinion_and_verdict(workflow_run("run-lane"), &file_only);
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
    assert!(!verdict.is_approved(), "{verdict:?}");

    // With the documents change beside it, the whole change still waits.
    let mixed = summary(
        &[
            ("tasks/feature/documents/design.md", FileChange::Added),
            ("tasks/feature/task.toml", FileChange::Modified),
        ],
        &["feature"],
    );
    let (opinion, verdict) = opinion_and_verdict(workflow_run("run-lane"), &mixed);
    assert_eq!(opinion, ApprovalOpinion::NoOpinion);
    assert!(!verdict.is_approved(), "{verdict:?}");
}

#[test]
fn lanes_a_lane_runs_change_outside_its_owned_scope_waits_for_a_person() {
    for outside in [
        summary(
            &[("tasks/other/task.toml", FileChange::Modified)],
            &["other"],
        ),
        summary(&[("tasks/feature/plan.md", FileChange::Modified)], &[]),
        summary(&[("project.toml", FileChange::Modified)], &[]),
        summary(
            &[(
                "tasks/feature/documents/../../other/task.toml",
                FileChange::Modified,
            )],
            &[],
        ),
    ] {
        let (opinion, verdict) = opinion_and_verdict(workflow_run("run-lane"), &outside);
        assert_eq!(opinion, ApprovalOpinion::NoOpinion, "{outside:?}");
        assert!(!verdict.is_approved());
    }
}

#[test]
fn lanes_a_lane_preparing_its_task_ahead_lands_inside_it_but_a_stopped_owner_waits() {
    let with_owner = |state: OwnerState| {
        let mut lanes = lanes_state();
        lanes.owners.get_mut(&TaskId::new("feature")).unwrap().state = state;
        hook_with(lanes)
    };
    let (opinion, verdict) = judged_by(
        &with_owner(OwnerState::Ahead),
        workflow_run("run-lane"),
        &inside(),
    );
    assert!(
        matches!(opinion, ApprovalOpinion::Approve(_)),
        "{opinion:?}"
    );
    assert!(verdict.is_approved(), "{verdict:?}");
    for state in [
        OwnerState::Stopped,
        OwnerState::Halted,
        OwnerState::Prepared,
        OwnerState::Done,
    ] {
        let (opinion, verdict) = judged_by(&with_owner(state), workflow_run("run-lane"), &inside());
        assert_eq!(opinion, ApprovalOpinion::NoOpinion, "{state}");
        assert!(!verdict.is_approved(), "{state}: {verdict:?}");
    }
}
