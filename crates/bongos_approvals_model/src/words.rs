use crate::RowState;

/// How a surface colors an approval's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    Normal,
    Warning,
    Error,
    Muted,
}

/// An approval as people see it, the input to [`words`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalView<'a> {
    /// A person's approval row. `approver` names the person for verified and weaker states; an
    /// unverified state names its `claimed_by`.
    Row {
        approver: &'a str,
        state: &'a RowState,
    },
    /// A row written from a person's earlier grant that verified.
    Grant { approver: &'a str },
    /// A change that landed before approvals were signed.
    PreFeature,
}

/// The one set of words for an approval's state, used by every surface. Unverified words never
/// say "approved by" and never carry a check mark.
pub fn words(view: &ApprovalView) -> (String, Tone) {
    match view {
        ApprovalView::Row { approver, state } => match state {
            RowState::Verified { .. } => (
                format!("approved by {approver}, verified (signed with {approver}'s approval key)"),
                Tone::Normal,
            ),
            RowState::Weaker { reason, .. } => (
                format!("approved by {approver}, weaker verification: {reason}"),
                Tone::Warning,
            ),
            RowState::Unverified { claimed_by, reason } => (
                format!("unverified approval claimed by {claimed_by}: {reason}"),
                Tone::Error,
            ),
        },
        ApprovalView::Grant { approver } => (
            format!("approved by {approver}'s earlier grant, verified"),
            Tone::Normal,
        ),
        ApprovalView::PreFeature => (
            "no verifiable approval (landed before approval integrity)".to_string(),
            Tone::Muted,
        ),
    }
}
