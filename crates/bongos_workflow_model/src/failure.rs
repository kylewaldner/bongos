//! Why a node or a run failed: the closed list of failure reasons and the class of each, definition
//! or task. Only definition failures are worth healing by editing the definition; a task failure is
//! the world (the machine, the network, the repository, the task's content) and is retried as is.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::record::NodeKey;

/// Who is likely to blame for a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The definition's text is the likely cause.
    Definition,
    /// The task or the world is the likely cause.
    Task,
}

/// Every reason a node or run can fail. The list is closed: nothing fails without one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    NodeOutputInvalid,
    AgentExitedNonzero,
    ProgramExitedNonzero,
    ProgramResultInvalid,
    TurnCapHit,
    CostCapHit,
    TimeCapHit,
    /// A program's captured output passed its cap. Not in the product's list: the program-node
    /// design names it, and it is always a task failure.
    OutputCapHit,
    PermissionDenied,
    AgentNotInstalledOrLoggedOut,
    ProgramInterpreterMissing,
    McpServerUnavailable,
    WorktreeUnavailable,
    MergeConflict,
    NoRouteForResult,
    LoopBoundReached,
    BranchLimitExceeded,
    WaitTimedOut,
    GateCancelled,
    OutcomePromisedNotDelivered,
    InterruptedNotResumed,
    /// The run reached a failure end of its definition (the workflow itself chose to fail). Not
    /// in the product's list: the engine names it so a failed run always records why.
    FailureEndReached,
}

impl FailureReason {
    pub const ALL: [FailureReason; 22] = [
        Self::NodeOutputInvalid,
        Self::AgentExitedNonzero,
        Self::ProgramExitedNonzero,
        Self::ProgramResultInvalid,
        Self::TurnCapHit,
        Self::CostCapHit,
        Self::TimeCapHit,
        Self::OutputCapHit,
        Self::PermissionDenied,
        Self::AgentNotInstalledOrLoggedOut,
        Self::ProgramInterpreterMissing,
        Self::McpServerUnavailable,
        Self::WorktreeUnavailable,
        Self::MergeConflict,
        Self::NoRouteForResult,
        Self::LoopBoundReached,
        Self::BranchLimitExceeded,
        Self::WaitTimedOut,
        Self::GateCancelled,
        Self::OutcomePromisedNotDelivered,
        Self::InterruptedNotResumed,
        Self::FailureEndReached,
    ];

    /// The text the run view and the run record show.
    pub fn label(self) -> &'static str {
        match self {
            Self::NodeOutputInvalid => "node output invalid",
            Self::AgentExitedNonzero => "agent exited nonzero",
            Self::ProgramExitedNonzero => "program exited nonzero",
            Self::ProgramResultInvalid => "program result invalid",
            Self::TurnCapHit => "turn cap hit",
            Self::CostCapHit => "cost cap hit",
            Self::TimeCapHit => "time cap hit",
            Self::OutputCapHit => "output cap hit",
            Self::PermissionDenied => "permission denied",
            Self::AgentNotInstalledOrLoggedOut => "agent not installed or logged out",
            Self::ProgramInterpreterMissing => "program interpreter missing",
            Self::McpServerUnavailable => "mcp server unavailable",
            Self::WorktreeUnavailable => "worktree unavailable",
            Self::MergeConflict => "merge conflict",
            Self::NoRouteForResult => "no route for a result",
            Self::LoopBoundReached => "loop bound reached",
            Self::BranchLimitExceeded => "branch limit exceeded",
            Self::WaitTimedOut => "wait timed out",
            Self::GateCancelled => "gate cancelled",
            Self::OutcomePromisedNotDelivered => "outcome promised but not delivered",
            Self::InterruptedNotResumed => "interrupted and not resumed",
            Self::FailureEndReached => "failure end reached",
        }
    }

    /// Whether this is a turn, cost or time cap, whose class depends on the node's retries.
    pub fn is_retry_dependent_cap(self) -> bool {
        matches!(self, Self::TurnCapHit | Self::CostCapHit | Self::TimeCapHit)
    }

    /// The class of this reason. A turn, cost or time cap is a task failure here; use
    /// [`FailureReason::class_for_cap`] when it is known whether every retry hit it.
    pub fn class(self) -> FailureClass {
        match self {
            Self::NodeOutputInvalid
            | Self::PermissionDenied
            | Self::OutcomePromisedNotDelivered
            | Self::ProgramResultInvalid
            | Self::NoRouteForResult
            | Self::BranchLimitExceeded => FailureClass::Definition,
            Self::AgentExitedNonzero
            | Self::ProgramExitedNonzero
            | Self::TurnCapHit
            | Self::CostCapHit
            | Self::TimeCapHit
            | Self::OutputCapHit
            | Self::AgentNotInstalledOrLoggedOut
            | Self::ProgramInterpreterMissing
            | Self::McpServerUnavailable
            | Self::WorktreeUnavailable
            | Self::MergeConflict
            | Self::LoopBoundReached
            | Self::WaitTimedOut
            | Self::GateCancelled
            | Self::InterruptedNotResumed
            | Self::FailureEndReached => FailureClass::Task,
        }
    }

    /// The class when it is known whether every retry of the node hit this reason: a turn, cost
    /// or time cap is a definition failure only when every retry hit it. Other reasons ignore
    /// `every_retry_hit`.
    pub fn class_for_cap(self, every_retry_hit: bool) -> FailureClass {
        if self.is_retry_dependent_cap() && every_retry_hit {
            FailureClass::Definition
        } else {
            self.class()
        }
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One failure as the run record keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub reason: FailureReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeKey>,
    #[serde(default)]
    pub detail: String,
    pub class: FailureClass,
}

impl Failure {
    /// A failure with the reason's own class (a cap counts as a task failure).
    pub fn new(reason: FailureReason, node: Option<NodeKey>, detail: impl Into<String>) -> Self {
        Self {
            reason,
            node,
            detail: detail.into(),
            class: reason.class(),
        }
    }

    /// A failure whose class takes the node's retries into account ([`FailureReason::class_for_cap`]).
    pub fn after_retries(
        reason: FailureReason,
        node: Option<NodeKey>,
        detail: impl Into<String>,
        every_retry_hit: bool,
    ) -> Self {
        Self {
            class: reason.class_for_cap(every_retry_hit),
            ..Self::new(reason, node, detail)
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.reason)?;
        if let Some(node) = &self.node {
            write!(formatter, " at {node}")?;
        }
        if !self.detail.is_empty() {
            write!(formatter, ": {}", self.detail)?;
        }
        Ok(())
    }
}
