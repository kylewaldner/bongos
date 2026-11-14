use bongos_work_model::{ProjectFile, parse_project};

use crate::checks::{
    ApprovalBasis, ApprovalOpinion, ApprovalPolicy, ApprovalVerdict, AutomaticRule, Judgment,
    PersonApproval, PersonRule, approval_policy_problems, judge, own_subtree, rollback_opinion,
};
use crate::test_trees::{PROJECT, tree};
use crate::{AuthorKind, summarize};

fn project(extra: &str) -> ProjectFile {
    parse_project(&format!("{PROJECT}{extra}")).unwrap()
}

fn row(basis: ApprovalBasis, amend: &str) -> PersonApproval {
    PersonApproval {
        basis,
        name: "Sam".to_string(),
        amend_patch_id: amend.to_string(),
    }
}

/// A judgment with no rows, opinions or rollback, under the default policy.
fn judgment<'a>(policy: &'a ApprovalPolicy, kind: AuthorKind) -> Judgment<'a> {
    Judgment {
        policy,
        author_kind: kind,
        rows: &[],
        amend_patch_id: "",
        rule_opinions: &[],
        hook_opinions: &[],
        rollback: None,
    }
}

fn needs(verdict: ApprovalVerdict) -> Vec<String> {
    match verdict {
        ApprovalVerdict::NeedsPerson { reasons } => reasons,
        other => panic!("expected to wait for a person, got {other:?}"),
    }
}

fn line(verdict: ApprovalVerdict) -> String {
    match verdict {
        ApprovalVerdict::Approved { line } => line,
        other => panic!("expected an approval, got {other:?}"),
    }
}

#[test]
fn the_policy_has_its_defaults() {
    let policy = ApprovalPolicy::parse(Some(&project(""))).unwrap();
    assert_eq!(policy, ApprovalPolicy::default());
    assert_eq!(policy.person, PersonRule::OnLand);
    assert_eq!(
        policy.rule_for(AuthorKind::Agent),
        Some(AutomaticRule::OwnSubtree)
    );
    assert_eq!(
        policy.rule_for(AuthorKind::Workflow),
        Some(AutomaticRule::DeclaredScope)
    );
    assert_eq!(
        policy.rule_for(AuthorKind::Healer),
        Some(AutomaticRule::IndependentReview)
    );
    for kind in [
        AuthorKind::Person,
        AuthorKind::PersonClaimed,
        AuthorKind::Unattributed,
    ] {
        assert_eq!(policy.rule_for(kind), None);
    }
    assert_eq!(
        ApprovalPolicy::parse(None).unwrap(),
        ApprovalPolicy::default()
    );
}

#[test]
fn the_policy_reads_each_key_and_person_turns_a_rule_off() {
    let policy = ApprovalPolicy::parse(Some(&project(
        "[approval]\nperson = \"approve\"\nagent = \"person\"\nworkflow = \"person\"\nhealer = \"person\"\n",
    )))
    .unwrap();
    assert_eq!(policy, ApprovalPolicy {
        person: PersonRule::Approve,
        ..ApprovalPolicy::strict()
    });
    // The project switch of the task builder turns the agent rule off too.
    let off = ApprovalPolicy::parse(Some(&project("auto_apply_proposals = false\n"))).unwrap();
    assert_eq!(off.agent, None);
    assert_eq!(off.workflow, Some(AutomaticRule::DeclaredScope));
}

#[test]
fn check_one_refuses_a_policy_it_cannot_read() {
    let reasons = |extra: &str| -> Vec<String> {
        approval_policy_problems(&project(extra))
            .into_iter()
            .map(|problem| {
                assert_eq!(problem.path, "project.toml");
                problem.reason
            })
            .collect()
    };
    assert!(reasons("[approval]\nagent = \"own-subtree\"\n").is_empty());
    assert_eq!(
        reasons("[approval]\nagent = \"always\"\n"),
        ["approval.agent must be \"own-subtree\" or \"person\""]
    );
    assert_eq!(
        reasons("[approval]\nunattributed = \"automatic\"\n"),
        [
            "approval.unattributed: unattributed changes and outside commits always need project: approve; it is not configurable"
        ]
    );
    assert_eq!(
        reasons("[approval]\nanyone = true\n"),
        ["approval: unknown key anyone"]
    );
    assert_eq!(
        reasons("approval = \"off\"\n"),
        ["[approval] must be a table"]
    );
}

#[test]
fn a_person_row_of_the_author_part_approves_and_names_the_person() {
    let policy = ApprovalPolicy::default();
    for (basis, words) in [
        (ApprovalBasis::PersonEdit, "approved: edited by Sam"),
        (ApprovalBasis::PersonLand, "approved: landed by Sam"),
        (ApprovalBasis::PersonApprove, "approved: approved by Sam"),
    ] {
        let rows = [row(basis, "")];
        let input = Judgment {
            rows: &rows,
            ..judgment(&policy, AuthorKind::PersonClaimed)
        };
        assert_eq!(line(judge(&input)), words);
    }
}

#[test]
fn without_a_row_no_author_kind_is_a_person() {
    let policy = ApprovalPolicy::default();
    for kind in [
        AuthorKind::Person,
        AuthorKind::PersonClaimed,
        AuthorKind::Unattributed,
        AuthorKind::Agent,
        AuthorKind::Workflow,
        AuthorKind::Healer,
    ] {
        assert!(
            !judge(&judgment(&policy, kind)).is_approved(),
            "{kind:?} approved itself"
        );
    }
}

#[test]
fn an_unattributed_change_needs_project_approve_even_with_an_edit_row() {
    let policy = ApprovalPolicy::default();
    let rows = [
        row(ApprovalBasis::PersonEdit, ""),
        row(ApprovalBasis::PersonLand, ""),
    ];
    let input = Judgment {
        rows: &rows,
        ..judgment(&policy, AuthorKind::Unattributed)
    };
    assert_eq!(
        needs(judge(&input)),
        ["an unattributed change needs a person"]
    );
    let approve = [row(ApprovalBasis::PersonApprove, "")];
    let input = Judgment {
        rows: &approve,
        ..judgment(&policy, AuthorKind::Unattributed)
    };
    assert!(judge(&input).is_approved());
}

#[test]
fn person_approve_policy_counts_only_project_approve() {
    let policy = ApprovalPolicy {
        person: PersonRule::Approve,
        ..ApprovalPolicy::default()
    };
    let rows = [row(ApprovalBasis::PersonLand, "")];
    let input = Judgment {
        rows: &rows,
        ..judgment(&policy, AuthorKind::PersonClaimed)
    };
    assert!(!judge(&input).is_approved());
}

#[test]
fn an_automatic_rule_approves_only_its_authors_kind_with_the_log_line() {
    let policy = ApprovalPolicy::default();
    let approve = [(
        "own-subtree".to_string(),
        ApprovalOpinion::Approve("agent change inside its own subtree".to_string()),
    )];
    let input = Judgment {
        rule_opinions: &approve,
        ..judgment(&policy, AuthorKind::Agent)
    };
    assert_eq!(
        line(judge(&input)),
        "approved automatically: agent change inside its own subtree"
    );
    // A hook that implements no rule cannot approve anything.
    let input = Judgment {
        hook_opinions: &approve,
        ..judgment(&policy, AuthorKind::Agent)
    };
    assert!(!judge(&input).is_approved());
}

#[test]
fn needs_person_wins_over_every_automatic_approval() {
    let policy = ApprovalPolicy::default();
    let approve = [(
        "own-subtree".to_string(),
        ApprovalOpinion::Approve("inside".to_string()),
    )];
    let protected = [(
        "protected-section".to_string(),
        ApprovalOpinion::NeedsPerson("touches the protected section".to_string()),
    )];
    let input = Judgment {
        rule_opinions: &approve,
        hook_opinions: &protected,
        rollback: Some(ApprovalOpinion::Approve("rollback".to_string())),
        ..judgment(&policy, AuthorKind::Agent)
    };
    assert_eq!(
        needs(judge(&input)),
        ["protected-section: touches the protected section"]
    );
}

#[test]
fn a_hook_that_trips_on_an_amendment_needs_the_person_again() {
    let policy = ApprovalPolicy::default();
    let protected = [(
        "protected-section".to_string(),
        ApprovalOpinion::NeedsPerson("touches the protected section".to_string()),
    )];
    // The person's edit was approved before the amendment existed.
    let edit = [row(ApprovalBasis::PersonEdit, "")];
    let input = Judgment {
        rows: &edit,
        amend_patch_id: "amended",
        hook_opinions: &protected,
        ..judgment(&policy, AuthorKind::PersonClaimed)
    };
    assert!(!judge(&input).is_approved());
    // Without the hook, the same row approves the amended change.
    let input = Judgment {
        rows: &edit,
        amend_patch_id: "amended",
        ..judgment(&policy, AuthorKind::PersonClaimed)
    };
    assert!(judge(&input).is_approved());
    // `project: approve` after the amendment covers it.
    let approve = [row(ApprovalBasis::PersonApprove, "amended")];
    let input = Judgment {
        rows: &approve,
        amend_patch_id: "amended",
        hook_opinions: &protected,
        ..judgment(&policy, AuthorKind::PersonClaimed)
    };
    assert_eq!(line(judge(&input)), "approved: approved by Sam");
}

#[test]
fn a_rule_turned_off_by_the_policy_says_so() {
    let policy = ApprovalPolicy::strict();
    assert_eq!(
        needs(judge(&judgment(&policy, AuthorKind::Workflow))),
        ["the project's approval policy asks a person"]
    );
    let default = ApprovalPolicy::default();
    assert_eq!(
        needs(judge(&judgment(&default, AuthorKind::Workflow))),
        ["no declared-scope rule approved it"]
    );
}

fn subtree_trees() -> (crate::DefinitionTree, crate::DefinitionTree) {
    let master = tree(&[("tasks/a", ""), ("tasks/a/tasks/a1", ""), ("tasks/b", "")]);
    let change = tree(&[
        ("tasks/a", ""),
        ("tasks/a/tasks/a1", ""),
        ("tasks/a/tasks/a2", ""),
        ("tasks/b", ""),
    ]);
    (master, change)
}

#[test]
fn own_subtree_approves_inside_the_chats_task_only() {
    let (master, change) = subtree_trees();
    let summary = summarize(&master, &change)
        .with_files(vec![("tasks/a/tasks/a2/task.toml".to_string(), crate::FileChange::Added)]);
    assert_eq!(
        own_subtree(Some("a"), &summary, &master, &change),
        ApprovalOpinion::Approve("agent change inside its own subtree".to_string())
    );
    assert_eq!(
        own_subtree(Some("b"), &summary, &master, &change),
        ApprovalOpinion::NeedsPerson("task a2 is outside the chat's task b".to_string())
    );
    assert_eq!(
        own_subtree(None, &summary, &master, &change),
        ApprovalOpinion::NeedsPerson("unknown chat".to_string())
    );
    assert_eq!(
        own_subtree(Some("ghost"), &summary, &master, &change),
        ApprovalOpinion::NeedsPerson("the chat's task ghost is not in master".to_string())
    );
}

#[test]
fn own_subtree_never_covers_project_level_files() {
    let (master, _) = subtree_trees();
    let summary = summarize(&master, &master).with_files(vec![(
        "project.toml".to_string(),
        crate::FileChange::Modified,
    )]);
    // Not even for the top group's chat: project.toml holds the approval policy.
    assert_eq!(
        own_subtree(Some("top"), &summary, &master, &master),
        ApprovalOpinion::NeedsPerson("project.toml is outside the chat's task top".to_string())
    );
}

#[test]
fn a_rollback_is_approved_only_as_the_exact_revert_of_an_approved_landing() {
    assert_eq!(
        rollback_opinion("m1", true, "tree-1", Some("tree-1")),
        ApprovalOpinion::Approve("rollback of move m1, covered by the move's approval".to_string())
    );
    assert!(matches!(
        rollback_opinion("m1", true, "tree-1", Some("tree-2")),
        ApprovalOpinion::NeedsPerson(_)
    ));
    assert!(matches!(
        rollback_opinion("m1", true, "tree-1", None),
        ApprovalOpinion::NeedsPerson(_)
    ));
    assert!(matches!(
        rollback_opinion("m1", false, "tree-1", Some("tree-1")),
        ApprovalOpinion::NeedsPerson(_)
    ));
    let policy = ApprovalPolicy::default();
    let input = Judgment {
        rollback: Some(rollback_opinion("m1", true, "t", Some("t"))),
        ..judgment(&policy, AuthorKind::PersonClaimed)
    };
    assert_eq!(
        line(judge(&input)),
        "approved: rollback of move m1, covered by the move's approval"
    );
}
