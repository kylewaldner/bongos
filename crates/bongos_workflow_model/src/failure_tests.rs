use std::collections::HashSet;

use crate::failure::{Failure, FailureClass, FailureReason};
use crate::record::NodeKey;

use FailureClass::{Definition, Task};
use FailureReason::*;

/// Every reason with its class when not every retry hit it, from the product's closed list.
const EXPECTED: [(FailureReason, &str, FailureClass); 22] = [
    (NodeOutputInvalid, "node output invalid", Definition),
    (AgentExitedNonzero, "agent exited nonzero", Task),
    (ProgramExitedNonzero, "program exited nonzero", Task),
    (ProgramResultInvalid, "program result invalid", Definition),
    (TurnCapHit, "turn cap hit", Task),
    (CostCapHit, "cost cap hit", Task),
    (TimeCapHit, "time cap hit", Task),
    (OutputCapHit, "output cap hit", Task),
    (PermissionDenied, "permission denied", Definition),
    (
        AgentNotInstalledOrLoggedOut,
        "agent not installed or logged out",
        Task,
    ),
    (
        ProgramInterpreterMissing,
        "program interpreter missing",
        Task,
    ),
    (McpServerUnavailable, "mcp server unavailable", Task),
    (WorktreeUnavailable, "worktree unavailable", Task),
    (MergeConflict, "merge conflict", Task),
    (NoRouteForResult, "no route for a result", Definition),
    (LoopBoundReached, "loop bound reached", Task),
    (BranchLimitExceeded, "branch limit exceeded", Definition),
    (WaitTimedOut, "wait timed out", Task),
    (GateCancelled, "gate cancelled", Task),
    (
        OutcomePromisedNotDelivered,
        "outcome promised but not delivered",
        Definition,
    ),
    (InterruptedNotResumed, "interrupted and not resumed", Task),
    (FailureEndReached, "failure end reached", Task),
];

#[test]
fn every_reason_has_its_class_and_text() {
    assert_eq!(
        EXPECTED.map(|(reason, _, _)| reason),
        FailureReason::ALL,
        "the table covers every reason in order"
    );
    for (reason, text, class) in EXPECTED {
        assert_eq!(reason.class(), class, "{text}");
        assert_eq!(reason.class_for_cap(false), class, "{text}");
        assert_eq!(reason.to_string(), text);
    }
}

#[test]
fn a_cap_is_a_definition_failure_only_when_every_retry_hit_it() {
    for reason in [TurnCapHit, CostCapHit, TimeCapHit] {
        assert_eq!(reason.class_for_cap(true), Definition);
        assert_eq!(reason.class_for_cap(false), Task);
    }
    assert_eq!(OutputCapHit.class_for_cap(true), Task);
    // Other reasons keep their class either way.
    assert_eq!(MergeConflict.class_for_cap(true), Task);
    assert_eq!(PermissionDenied.class_for_cap(false), Definition);
}

#[test]
fn serde_names_are_stable_and_distinct() {
    let mut names = HashSet::new();
    for reason in FailureReason::ALL {
        let name = serde_json::to_value(reason).unwrap();
        let name = name.as_str().unwrap().to_string();
        assert!(names.insert(name.clone()));
        assert_eq!(
            serde_json::from_value::<FailureReason>(serde_json::Value::String(name)).unwrap(),
            reason
        );
    }
    for (reason, name) in [
        (NodeOutputInvalid, "node_output_invalid"),
        (NoRouteForResult, "no_route_for_result"),
        (
            OutcomePromisedNotDelivered,
            "outcome_promised_not_delivered",
        ),
        (InterruptedNotResumed, "interrupted_not_resumed"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), name);
    }
}

#[test]
fn failure_records_round_trip() {
    let failure =
        Failure::after_retries(TurnCapHit, Some(NodeKey::new("write", 1)), "50 turns", true);
    assert_eq!(failure.class, Definition);
    assert_eq!(failure.to_string(), "turn cap hit at write#1: 50 turns");
    let json = serde_json::to_string(&failure).unwrap();
    assert_eq!(serde_json::from_str::<Failure>(&json).unwrap(), failure);
    assert_eq!(Failure::new(TurnCapHit, None, "").class, Task);
}
