//! Check 9, approval, as pure rules: the project's `[approval]` policy, the bases of a person's
//! approval, what an approval hook may say, the `own-subtree` and move rollback rules, and the
//! judgment over all of them.
//!
//! The judgment never looks at who a change claims to be written by to decide that a person
//! approved: only approval rows count for that, and the caller passes only the rows written for
//! the change's current author part. The author's kind only selects which automatic rule of the
//! policy may approve (an agent's, a workflow's or a healer's); a person, a claimed person and
//! an unattributed author have no automatic rule.

use serde::{Deserialize, Serialize};

use crate::definitions::{DefinitionTree, Problem};
use crate::layout::{DOCUMENTS_DIR, PLAN_FILE, PROJECT_FILE, TASK_FILE, TASKS_DIR};
use crate::{AuthorKind, ChangeSummary};
use bongos_work_model::ProjectFile;

/// The table of `project.toml` that holds the policy.
pub const APPROVAL_TABLE: &str = "approval";

/// How a person approved a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalBasis {
    /// A quick edit a person made from a view: the edit itself is the request to land it.
    PersonEdit,
    /// `project: land` by a person on the person's own change.
    PersonLand,
    /// `project: approve`.
    PersonApprove,
}

impl ApprovalBasis {
    /// How the log line names it.
    pub fn words(self) -> &'static str {
        match self {
            ApprovalBasis::PersonEdit => "edited by",
            ApprovalBasis::PersonLand => "landed by",
            ApprovalBasis::PersonApprove => "approved by",
        }
    }
}

/// What a person's own change needs: landing it (or, for a quick edit, the edit itself) or an
/// explicit `project: approve`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonRule {
    /// `person = "on-land"`, the default.
    OnLand,
    /// `person = "approve"`: only `project: approve` counts.
    Approve,
}

/// An automatic rule of the policy, decided by the approval hooks that implement it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutomaticRule {
    /// An agent's change inside its chat's task subtree.
    OwnSubtree,
    /// A workflow run's change inside the scope its landed definition declares.
    DeclaredScope,
    /// A healer's change after a confirmed independent review that says ship.
    IndependentReview,
}

impl AutomaticRule {
    pub fn word(self) -> &'static str {
        match self {
            AutomaticRule::OwnSubtree => "own-subtree",
            AutomaticRule::DeclaredScope => "declared-scope",
            AutomaticRule::IndependentReview => "independent-review",
        }
    }
}

/// The value of a policy key that turns its automatic rule off.
const PERSON_WORD: &str = "person";

/// A project's `[approval]` policy. Unattributed changes and outside commits always need
/// `project: approve`; nothing configures that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalPolicy {
    pub person: PersonRule,
    /// `None`: an agent's change always needs a person.
    pub agent: Option<AutomaticRule>,
    pub workflow: Option<AutomaticRule>,
    pub healer: Option<AutomaticRule>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            person: PersonRule::OnLand,
            agent: Some(AutomaticRule::OwnSubtree),
            workflow: Some(AutomaticRule::DeclaredScope),
            healer: Some(AutomaticRule::IndependentReview),
        }
    }
}

impl ApprovalPolicy {
    /// No automatic rule: every change needs a person. Used when master's policy cannot be
    /// read, which check 1 never lets land.
    pub fn strict() -> Self {
        Self {
            person: PersonRule::OnLand,
            agent: None,
            workflow: None,
            healer: None,
        }
    }

    /// The policy of `project` (master's), the defaults for what it leaves out. The project
    /// switch `auto_apply_proposals = false` turns the agent rule off.
    pub fn parse(project: Option<&ProjectFile>) -> Result<Self, Vec<String>> {
        let mut policy = Self::default();
        let mut problems = Vec::new();
        if let Some(table) = project.and_then(|project| project.extra.get(APPROVAL_TABLE)) {
            let Some(table) = table.as_table() else {
                return Err(vec![format!("[{APPROVAL_TABLE}] must be a table")]);
            };
            for (key, value) in table {
                let word = value.as_str();
                let mut bad = |allowed: &str| {
                    problems.push(format!("{APPROVAL_TABLE}.{key} must be {allowed}"));
                };
                match key.as_str() {
                    "person" => match word {
                        Some("on-land") => policy.person = PersonRule::OnLand,
                        Some("approve") => policy.person = PersonRule::Approve,
                        _ => bad("\"on-land\" or \"approve\""),
                    },
                    "agent" => match word {
                        Some("own-subtree") => policy.agent = Some(AutomaticRule::OwnSubtree),
                        Some(PERSON_WORD) => policy.agent = None,
                        _ => bad("\"own-subtree\" or \"person\""),
                    },
                    "workflow" => match word {
                        Some("declared-scope") => {
                            policy.workflow = Some(AutomaticRule::DeclaredScope)
                        }
                        Some(PERSON_WORD) => policy.workflow = None,
                        _ => bad("\"declared-scope\" or \"person\""),
                    },
                    "healer" => match word {
                        Some("independent-review") => {
                            policy.healer = Some(AutomaticRule::IndependentReview)
                        }
                        Some(PERSON_WORD) => policy.healer = None,
                        _ => bad("\"independent-review\" or \"person\""),
                    },
                    "unattributed" | "outside" => problems.push(format!(
                        "{APPROVAL_TABLE}.{key}: unattributed changes and outside commits always \
                         need project: approve; it is not configurable"
                    )),
                    other => problems.push(format!("{APPROVAL_TABLE}: unknown key {other}")),
                }
            }
        }
        if project.and_then(|project| project.auto_apply_proposals) == Some(false) {
            policy.agent = None;
        }
        if problems.is_empty() {
            Ok(policy)
        } else {
            Err(problems)
        }
    }

    /// The automatic rule that may approve a change by an author of `kind`.
    pub fn rule_for(&self, kind: AuthorKind) -> Option<AutomaticRule> {
        match kind {
            AuthorKind::Agent => self.agent,
            AuthorKind::Workflow => self.workflow,
            AuthorKind::Healer => self.healer,
            AuthorKind::Person | AuthorKind::PersonClaimed | AuthorKind::Unattributed => None,
        }
    }
}

/// Check 1's part of the policy: what `project`'s `[approval]` table cannot be.
pub fn approval_policy_problems(project: &ProjectFile) -> Vec<Problem> {
    match ApprovalPolicy::parse(Some(project)) {
        Ok(_) => Vec::new(),
        Err(reasons) => reasons
            .into_iter()
            .map(|reason| Problem {
                path: PROJECT_FILE.to_string(),
                reason,
            })
            .collect(),
    }
}

/// What one approval hook says about a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalOpinion {
    /// The hook's rule approves; the reason goes into the log line.
    Approve(String),
    /// A person must approve; this wins over every automatic approval.
    NeedsPerson(String),
    NoOpinion,
}

/// One approval row of the change's current author part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonApproval {
    pub basis: ApprovalBasis,
    pub name: String,
    /// The patch id of the amendments on top of the author's part when the row was written;
    /// empty when there were none.
    pub amend_patch_id: String,
}

/// What the approval judgment reads.
pub struct Judgment<'a> {
    pub policy: &'a ApprovalPolicy,
    /// The stored author's kind: it only selects the automatic rule.
    pub author_kind: AuthorKind,
    /// The approval rows written for the change's current author part, and only those.
    pub rows: &'a [PersonApproval],
    /// The patch id of the amendments on top of the author's part now; empty when none.
    pub amend_patch_id: &'a str,
    /// The opinions of the hooks of [`ApprovalPolicy::rule_for`] the author's kind, by hook
    /// name; empty when the policy turns the rule off.
    pub rule_opinions: &'a [(String, ApprovalOpinion)],
    /// The opinions of the hooks that implement no rule, by hook name: their `Approve` counts
    /// for nothing, their `NeedsPerson` holds the change for a person.
    pub hook_opinions: &'a [(String, ApprovalOpinion)],
    /// The move rollback rule's opinion, when the change is a recorded rollback.
    pub rollback: Option<ApprovalOpinion>,
}

/// Whether the change may land, and the log line that says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalVerdict {
    Approved { line: String },
    NeedsPerson { reasons: Vec<String> },
}

impl ApprovalVerdict {
    pub fn is_approved(&self) -> bool {
        matches!(self, ApprovalVerdict::Approved { .. })
    }

    /// What a waiting change shows.
    pub fn waiting_reason(&self) -> Option<String> {
        match self {
            ApprovalVerdict::Approved { .. } => None,
            ApprovalVerdict::NeedsPerson { reasons } => Some(format!(
                "waiting for approval: {}; run project: approve",
                reasons.join("; ")
            )),
        }
    }
}

fn person_line(row: &PersonApproval) -> String {
    format!("approved: {} {}", row.basis.words(), row.name)
}

/// Check 9's judgment. A `NeedsPerson` from any hook wins over every automatic approval and is
/// satisfied only by a person row written for exactly the current amendments too (so an
/// amendment that trips a hook after the person's row needs the person again). Without one, a
/// person row of the current author part approves, then the move rollback rule, then the
/// author's automatic rule; anything else waits for a person.
pub fn judge(input: &Judgment) -> ApprovalVerdict {
    let counts = |row: &&PersonApproval| match row.basis {
        ApprovalBasis::PersonApprove => true,
        // Unattributed changes always need project: approve.
        ApprovalBasis::PersonEdit | ApprovalBasis::PersonLand => {
            input.policy.person == PersonRule::OnLand
                && input.author_kind != AuthorKind::Unattributed
        }
    };
    let usable: Vec<&PersonApproval> = input.rows.iter().filter(counts).collect();
    let mut needs: Vec<String> = input
        .hook_opinions
        .iter()
        .chain(input.rule_opinions)
        .filter_map(|(name, opinion)| match opinion {
            ApprovalOpinion::NeedsPerson(reason) => Some(format!("{name}: {reason}")),
            _ => None,
        })
        .collect();
    if let Some(ApprovalOpinion::NeedsPerson(reason)) = &input.rollback {
        needs.push(reason.clone());
    }
    if !needs.is_empty() {
        return match usable
            .iter()
            .find(|row| row.amend_patch_id == input.amend_patch_id)
        {
            Some(row) => ApprovalVerdict::Approved {
                line: person_line(row),
            },
            None => ApprovalVerdict::NeedsPerson { reasons: needs },
        };
    }
    if let Some(row) = usable.first() {
        return ApprovalVerdict::Approved {
            line: person_line(row),
        };
    }
    if let Some(ApprovalOpinion::Approve(reason)) = &input.rollback {
        return ApprovalVerdict::Approved {
            line: format!("approved: {reason}"),
        };
    }
    if let Some((_, ApprovalOpinion::Approve(reason))) = input
        .rule_opinions
        .iter()
        .find(|(_, opinion)| matches!(opinion, ApprovalOpinion::Approve(_)))
    {
        return ApprovalVerdict::Approved {
            line: format!("approved automatically: {reason}"),
        };
    }
    let reason = match (input.author_kind, input.policy.rule_for(input.author_kind)) {
        (AuthorKind::Unattributed, _) => "an unattributed change needs a person".to_string(),
        (AuthorKind::Person | AuthorKind::PersonClaimed, _) => {
            "no person approved this version of the change".to_string()
        }
        (_, None) => "the project's approval policy asks a person".to_string(),
        (_, Some(rule)) => format!("no {} rule approved it", rule.word()),
    };
    ApprovalVerdict::NeedsPerson {
        reasons: vec![reason],
    }
}

/// Whether the file `path` (relative to `definitions/`) is inside the subtree of the task whose
/// folder is `folder`. The top group's subtree is its own files and every task, never the
/// project-level files (`project.toml`, which holds the policy, workflows, checks and the
/// registered folders).
fn in_subtree(folder: &str, path: &str) -> bool {
    if folder.is_empty() {
        return path == TASK_FILE
            || path == PLAN_FILE
            || path.starts_with(&format!("{TASKS_DIR}/"))
            || path.starts_with(&format!("{DOCUMENTS_DIR}/"));
    }
    path.starts_with(&format!("{folder}/"))
}

fn folder_in_subtree(folder: &str, task_folder: &str) -> bool {
    folder.is_empty() || task_folder == folder || task_folder.starts_with(&format!("{folder}/"))
}

/// The `own-subtree` rule: every task the change adds, removes or redefines (in both of its
/// places when it moved) and every file it changes is the chat's task or below it. `chat_task`
/// is the task the chat index holds for the author's chat, looked up by the caller in state the
/// author cannot write; `None` (no chat id, or one the index does not know) needs a person.
pub fn own_subtree(
    chat_task: Option<&str>,
    summary: &ChangeSummary,
    master: &DefinitionTree,
    change: &DefinitionTree,
) -> ApprovalOpinion {
    let Some(chat_task) = chat_task else {
        return ApprovalOpinion::NeedsPerson("unknown chat".to_string());
    };
    let Some(root) = master.node(chat_task) else {
        return ApprovalOpinion::NeedsPerson(format!("the chat's task {chat_task} is not in master"));
    };
    let folder = root.folder.as_str();
    let touched = summary
        .tasks_added
        .iter()
        .chain(&summary.tasks_removed)
        .chain(&summary.tasks_redefined)
        .chain(&summary.reparented);
    for id in touched {
        for tree in [master, change] {
            if let Some(node) = tree.node(id)
                && !folder_in_subtree(folder, &node.folder)
            {
                return ApprovalOpinion::NeedsPerson(format!(
                    "task {id} is outside the chat's task {chat_task}"
                ));
            }
        }
    }
    if let Some((path, _)) = summary
        .files
        .iter()
        .find(|(path, _)| !in_subtree(folder, path))
    {
        return ApprovalOpinion::NeedsPerson(format!(
            "{path} is outside the chat's task {chat_task}"
        ));
    }
    ApprovalOpinion::Approve("agent change inside its own subtree".to_string())
}

/// The move rollback rule: a change the move coordinator recorded as the rollback of move
/// `move_id` is approved when the move's target landing was approved and the rollback's author
/// part has exactly the tree of the revert of the target's landing on the master it is based on.
pub fn rollback_opinion(
    move_id: &str,
    target_approved: bool,
    author_tree: &str,
    revert_tree: Option<&str>,
) -> ApprovalOpinion {
    if !target_approved {
        return ApprovalOpinion::NeedsPerson(format!(
            "the landing of move {move_id} was not approved"
        ));
    }
    match revert_tree {
        Some(revert) if revert == author_tree => ApprovalOpinion::Approve(format!(
            "rollback of move {move_id}, covered by the move's approval"
        )),
        _ => ApprovalOpinion::NeedsPerson(format!(
            "the rollback of move {move_id} is not the revert of its landing"
        )),
    }
}
