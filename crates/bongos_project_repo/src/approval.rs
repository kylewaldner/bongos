//! Check 9, approval: always on, before the queue, and never skipped by any caller. `land` has no
//! parameter that bypasses it; the only way past "needs a person" is an `approvals` row written
//! from a person's attestation, or an automatic rule of the project's `[approval]` policy.
//!
//! A person approval row is written once, by the process that consumed the person's
//! `PersonAtKeyboard` (or by the host for a verified window, `host_boundary.rs`), against the
//! change id and the `git patch-id` of the author's part: the diff from the change's base to its
//! last author commit, the amend commits the landing engine recorded left out. The check, every
//! time it runs, reads only the rows of the change whose author patch id is the current one, so
//! master moving, a rebase and a bounded amendment keep a person's approval while a different
//! author part needs a new one. It never reads who the change claims to be by to decide that a
//! person approved: a stored or received author is at most a claimed person and approves nothing.
//!
//! The author's kind only selects an automatic rule (an agent's, a workflow's, a healer's), which
//! the approval hooks that implement it decide. Every other hook judges the whole change,
//! amendments included, and a `NeedsPerson` from any of them waits for a person.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::checks::{
    APPROVAL_CHECK, ApprovalBasis, ApprovalOpinion, ApprovalPolicy, ApprovalVerdict, AutomaticRule,
    CheckOutcome, Judgment, LiveFacts, PersonApproval, judge, own_subtree, rollback_opinion,
};
use bongos_project_repo_model::{AuthorKind, ChangeMeta, ChangeSummary, DefinitionTree};
use bongos_work_model::commands::Context as WorkContext;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::changes::{ChangeRow, ChangeState};
use crate::check_run::{CheckResult, CheckRun};
use crate::create::ProjectRepo;
use crate::state::StateWriter;

/// The `kv` namespace where the move coordinator records the rollbacks it makes, keyed by the
/// rollback's change id. Nothing a caller supplies writes it.
pub const MOVE_ROLLBACK_NAMESPACE: &str = "move-rollback";

/// Why `project: approve` and `project: land` refuse without a person.
pub const NEEDS_A_WINDOW: &str = "approving needs a person at a bongos window";

/// What an approval hook reads: the change and the project by id (so a hook can look the change
/// up in its own state instead of trusting the author's claims), the summary, meta and parsed
/// definitions of master and of the full change (amendments included), and both materialized
/// trees, which stay valid while `judge` runs.
pub struct ApprovalInput<'a> {
    pub change_id: &'a str,
    pub project_id: &'a str,
    pub summary: &'a ChangeSummary,
    pub meta: &'a ChangeMeta,
    pub live: &'a LiveFacts,
    pub master: &'a DefinitionTree,
    pub change: &'a DefinitionTree,
    pub master_root: &'a Path,
    pub change_root: &'a Path,
}

/// A rule other features add to check 9. A hook that implements an automatic rule of the policy
/// (`own-subtree`, `declared-scope`, `independent-review`) is consulted only when the policy turns
/// that rule on for the change's author kind, and only such a hook can approve. Every other hook
/// judges every change; its `Approve` counts for nothing and its `NeedsPerson` holds the change
/// for a person. `judge` must be fast and deterministic, and must look the author's claims (run,
/// chat) up in state the author cannot write, answering `NeedsPerson` when that fails.
pub trait ApprovalHook: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn rule(&self) -> Option<AutomaticRule> {
        None
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion;
}

/// The registry of approval hooks.
pub struct ApprovalHooks;

impl ApprovalHooks {
    pub fn register(hook: Arc<dyn ApprovalHook>, cx: &mut gpui::App) {
        crate::landing_checks::register_approval_hook(hook, cx);
    }
}

/// Which task a chat works on, from the chat index; `None` for a chat it does not know.
pub type ChatTaskLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The `own-subtree` rule as a hook: an agent's change is approved inside the task subtree of its
/// chat, whose task comes from the chat index (`lookup`), never from the change. The chat itself
/// is bound only in this process: the host drops the chat of every author a peer sends
/// (`host_boundary.rs`), so a peer's agent change reads as from no chat and waits for a person.
pub struct OwnSubtreeHook {
    lookup: ChatTaskLookup,
}

impl OwnSubtreeHook {
    pub const NAME: &'static str = "own-subtree";

    pub fn new(lookup: ChatTaskLookup) -> Self {
        Self { lookup }
    }
}

impl ApprovalHook for OwnSubtreeHook {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn rule(&self) -> Option<AutomaticRule> {
        Some(AutomaticRule::OwnSubtree)
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        let chat_task = input
            .meta
            .author
            .chat
            .as_deref()
            .and_then(|chat| (self.lookup)(chat));
        own_subtree(
            chat_task.as_deref(),
            input.summary,
            input.master,
            input.change,
        )
    }
}

/// Proof that a person, at a window, asks for an approval: built only by consuming a
/// `PersonAtKeyboard` (or a context still holding one), or by the host for a peer it verified.
/// It is neither `Clone` nor serializable, so it is used once.
pub(crate) struct PersonGrant {
    name: String,
}

impl PersonGrant {
    /// Takes the attestation out of `ctx`; `None` when it has none (a clone, a context built
    /// without a window) or its author is not a person at the keyboard.
    pub(crate) fn from_context(ctx: &mut WorkContext) -> Option<Self> {
        if ctx.author.kind() != AuthorKind::Person {
            return None;
        }
        ctx.take_attestation()?;
        Some(Self {
            name: ctx.author.name.clone(),
        })
    }

    /// The host's grant for a window it verified (`host_boundary.rs`).
    pub(crate) fn for_verified_peer(name: String) -> Self {
        Self { name }
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

/// One approval row as the views show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRow {
    pub change_id: String,
    pub author_patch_id: String,
    pub amend_patch_id: String,
    /// The branch tip the person approved; empty in a row written before it was kept.
    pub commit: String,
    pub basis: ApprovalBasis,
    pub name: String,
    pub at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct ApprovalRecord {
    author_patch_id: String,
    #[serde(default)]
    amend_patch_id: String,
    #[serde(default)]
    commit: String,
    basis: ApprovalBasis,
    name: String,
    at: String,
}

/// A change's author part: its last author commit (the branch tip without the amend commits the
/// engine recorded), the base it is diffed from, and the patch ids of the author part and of the
/// amendments on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthorPart {
    pub(crate) last_author: String,
    pub(crate) patch_id: String,
    pub(crate) amend_patch_id: String,
}

impl ProjectRepo {
    /// The author part of `row` with its branch at `tip`, based on `master`.
    pub(crate) async fn author_part(
        &self,
        row: &ChangeRow,
        tip: &str,
        master: &str,
    ) -> Result<AuthorPart> {
        let repository = &self.location.repository;
        let last_author = self.last_author_commit(row, tip).await?;
        let base = self
            .git
            .run(repository, &["merge-base", &last_author, master])
            .await
            .with_context(|| format!("finding the base of change {}", row.change_id))?;
        let patch_id = self.git.patch_id(repository, &base, &last_author).await?;
        let amend_patch_id = if last_author == tip {
            String::new()
        } else {
            self.git.patch_id(repository, &last_author, tip).await?
        };
        Ok(AuthorPart {
            last_author,
            patch_id,
            amend_patch_id,
        })
    }

    /// The last author commit of `row` with its branch at `tip`. Amend commits sit on top of the
    /// author's commits, so the commits from the oldest listed one the branch holds up to `tip`
    /// are amendments only when `tip` is the newest listed one the branch holds and every one of
    /// them is listed; a commit the author stacked on top (or between) makes the whole tip the
    /// author's part, which no earlier approval matches. One listed but not on the branch (a
    /// crash between the record and the branch move) is skipped, as when amendments are dropped.
    /// A commit is an amendment only because the engine listed it, never because of its subject.
    async fn last_author_commit(&self, row: &ChangeRow, tip: &str) -> Result<String> {
        let repository = &self.location.repository;
        let mut on_branch = Vec::new();
        for amend in &row.amend_commits {
            if self.git.is_ancestor(repository, &amend.commit, tip).await {
                on_branch.push(amend.commit.as_str());
            }
        }
        let (Some(oldest), Some(newest)) = (on_branch.first(), on_branch.last()) else {
            return Ok(tip.to_string());
        };
        if *newest != tip {
            return Ok(tip.to_string());
        }
        let author = self
            .git
            .rev_parse(repository, &format!("{oldest}^"))
            .await?;
        let range = self
            .git
            .run(repository, &["rev-list", &format!("{author}..{tip}")])
            .await
            .with_context(|| format!("listing the amendments of change {}", row.change_id))?;
        let all_listed = range
            .lines()
            .all(|commit| row.amend_commits.iter().any(|amend| amend.commit == commit));
        Ok(if all_listed { author } else { tip.to_string() })
    }

    /// The tip of `row`'s branch.
    pub(crate) async fn branch_tip(&self, row: &ChangeRow) -> Result<String> {
        Ok(self
            .git
            .rev_parse(
                &self.location.repository,
                &format!("refs/heads/{}", row.branch),
            )
            .await?)
    }

    /// Every approval row of `change_id`, oldest first.
    pub fn approvals(&self, change_id: &str) -> Result<Vec<ApprovalRow>> {
        let records = self.read_state(|read| read.approvals(change_id))?;
        records
            .iter()
            .map(|record| {
                let stored: ApprovalRecord =
                    toml::from_str(record).context("reading an approval row")?;
                Ok(ApprovalRow {
                    change_id: change_id.to_string(),
                    author_patch_id: stored.author_patch_id,
                    amend_patch_id: stored.amend_patch_id,
                    commit: stored.commit,
                    basis: stored.basis,
                    name: stored.name,
                    at: DateTime::parse_from_rfc3339(&stored.at)
                        .context("reading the time of an approval")?
                        .with_timezone(&Utc),
                })
            })
            .collect()
    }

    /// Writes a person's approval of `row` as its branch is now. The grant is consumed.
    pub(crate) async fn record_person_approval(
        &self,
        writer: &StateWriter,
        row: &ChangeRow,
        basis: ApprovalBasis,
        grant: PersonGrant,
        now: DateTime<Utc>,
    ) -> Result<ApprovalRow> {
        let (text, approval) = self.person_approval(row, basis, grant, now).await?;
        writer.transact(|tx| tx.add_approval(&row.change_id, &text))?;
        self.approval_recorded(&approval, row);
        Ok(approval)
    }

    /// The approval row of `grant` on `row`'s branch tip, and its stored text, without writing
    /// it: a caller adds it in its own transaction, then calls [`Self::approval_recorded`].
    pub(crate) async fn person_approval(
        &self,
        row: &ChangeRow,
        basis: ApprovalBasis,
        grant: PersonGrant,
        now: DateTime<Utc>,
    ) -> Result<(String, ApprovalRow)> {
        if !matches!(row.state, ChangeState::Open | ChangeState::Requested) {
            bail!("change {} is no longer open", row.change_id);
        }
        let tip = self.branch_tip(row).await?;
        let master = self.shown_commit().await?.0;
        let part = self.author_part(row, &tip, &master).await?;
        let record = ApprovalRecord {
            author_patch_id: part.patch_id.clone(),
            amend_patch_id: part.amend_patch_id.clone(),
            commit: tip.clone(),
            basis,
            name: grant.name.clone(),
            at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        let text = toml::to_string(&record).context("serializing an approval")?;
        Ok((
            text,
            ApprovalRow {
                change_id: row.change_id.clone(),
                author_patch_id: part.patch_id,
                amend_patch_id: part.amend_patch_id,
                commit: tip,
                basis,
                name: grant.name,
                at: now,
            },
        ))
    }

    /// Logs an approval written and wakes the landings waiting for one.
    pub(crate) fn approval_recorded(&self, approval: &ApprovalRow, row: &ChangeRow) {
        log::info!(
            "approved: {} {} (change {} on {} at {})",
            approval.basis.words(),
            approval.name,
            row.change_id,
            row.branch,
            short_commit(&approval.commit)
        );
        self.engine.signal.notify();
    }

    /// `project: approve` of `change_id` by the person whose context `ctx` is: refused unless the
    /// context still holds the attestation of a person at a window (a clone holds none). The
    /// approval is of the change as its branch is now; a landing waiting for it goes on.
    pub async fn approve(
        &self,
        change_id: &str,
        mut ctx: WorkContext,
        now: DateTime<Utc>,
    ) -> Result<ApprovalRow> {
        let grant = PersonGrant::from_context(&mut ctx).ok_or_else(|| anyhow!(NEEDS_A_WINDOW))?;
        self.approve_with(change_id, grant, now).await
    }

    pub(crate) async fn approve_with(
        &self,
        change_id: &str,
        grant: PersonGrant,
        now: DateTime<Utc>,
    ) -> Result<ApprovalRow> {
        let writer = self.writer_or_error()?;
        let row = self
            .change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
        self.record_person_approval(&writer, &row, ApprovalBasis::PersonApprove, grant, now)
            .await
    }

    /// The person approval of `project: land` (a `PersonLand` row), written only when the change
    /// is the person's own; a person landing someone else's change still needs `project: approve`.
    pub(crate) async fn record_land_approval(
        &self,
        writer: &StateWriter,
        row: &ChangeRow,
        grant: PersonGrant,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let own = matches!(
            row.author.kind(),
            AuthorKind::Person | AuthorKind::PersonClaimed
        ) && row.author.name == grant.name;
        if own {
            self.record_person_approval(writer, row, ApprovalBasis::PersonLand, grant, now)
                .await?;
        } else {
            log::info!(
                "{} landed change {} of {}; it needs project: approve",
                grant.name,
                row.change_id,
                row.author.name
            );
        }
        Ok(())
    }

    /// The opinion of the move rollback rule, when the coordinator recorded `row` as a rollback.
    async fn rollback(
        &self,
        row: &ChangeRow,
        part: &AuthorPart,
        master: &str,
    ) -> Result<Option<ApprovalOpinion>> {
        #[derive(Deserialize)]
        struct Rollback {
            move_id: String,
            target_commit: String,
            #[serde(default)]
            target_approved: bool,
        }
        let Some(value) =
            self.read_state(|read| read.kv(MOVE_ROLLBACK_NAMESPACE).get(&row.change_id))?
        else {
            return Ok(None);
        };
        let Ok(record) = value.try_into::<Rollback>() else {
            return Ok(Some(ApprovalOpinion::NeedsPerson(
                "the move's record of this rollback cannot be read".to_string(),
            )));
        };
        let repository = &self.location.repository;
        let author_tree = self.git.tree_of(repository, &part.last_author).await?;
        let revert = self
            .git
            .revert_tree(repository, &record.target_commit, master)
            .await?;
        Ok(Some(rollback_opinion(
            &record.move_id,
            record.target_approved,
            &author_tree,
            revert.as_deref(),
        )))
    }

    /// Check 9 on the change `row` whose branch is at `tip`, checked by `run`: its result (stored
    /// under the author patch id) and the verdict.
    pub(crate) async fn judge_approval(
        &self,
        writer: &StateWriter,
        row: &ChangeRow,
        run: &mut CheckRun,
        tip: &str,
    ) -> Result<(ApprovalVerdict, CheckResult)> {
        let master = run.facts.master_commit.clone();
        let part = self.author_part(row, tip, &master).await?;
        let rows: Vec<PersonApproval> = self
            .approvals(&row.change_id)?
            .into_iter()
            .filter(|approval| approval.author_patch_id == part.patch_id)
            .map(|approval| PersonApproval {
                basis: approval.basis,
                name: approval.name,
                amend_patch_id: approval.amend_patch_id,
            })
            .collect();
        let master_project = run
            .facts
            .master
            .definitions
            .as_ref()
            .ok()
            .and_then(|tree| tree.project.as_ref());
        let policy = ApprovalPolicy::parse(master_project).unwrap_or_else(|problems| {
            log::error!(
                "master's approval policy cannot be read ({}); every change needs a person",
                problems.join("; ")
            );
            ApprovalPolicy::strict()
        });
        let kind = row.author.kind();
        let active = policy.rule_for(kind);
        let mut rule_opinions = Vec::new();
        let mut hook_opinions = Vec::new();
        let verdict = match (&run.facts.master.definitions, &run.facts.new.definitions) {
            (Ok(master_tree), Ok(change_tree)) => {
                let (master_tree, change_tree) = (master_tree.clone(), change_tree.clone());
                self.pin(run).await?;
                // The hooks see the change's stored author, the one whose kind chose the rule,
                // whatever author the landing request named.
                let mut meta = run.meta.clone();
                meta.author = row.author.clone();
                let check_input = run.input();
                let input = ApprovalInput {
                    change_id: &row.change_id,
                    project_id: &self.location.id,
                    summary: &run.facts.summary,
                    meta: &meta,
                    live: &run.live,
                    master: &master_tree,
                    change: &change_tree,
                    master_root: check_input.master_root,
                    change_root: check_input.root,
                };
                for hook in &run.registry.approval_hooks {
                    match hook.rule() {
                        None => hook_opinions.push((hook.name().to_string(), hook.judge(&input))),
                        Some(rule) if Some(rule) == active => {
                            rule_opinions.push((hook.name().to_string(), hook.judge(&input)))
                        }
                        Some(_) => {}
                    }
                }
                let rollback = self.rollback(row, &part, &master).await?;
                judge(&Judgment {
                    policy: &policy,
                    author_kind: kind,
                    rows: &rows,
                    amend_patch_id: &part.amend_patch_id,
                    rule_opinions: &rule_opinions,
                    hook_opinions: &hook_opinions,
                    rollback,
                })
            }
            _ => ApprovalVerdict::NeedsPerson {
                reasons: vec!["the definitions do not parse".to_string()],
            },
        };
        let outcome = match verdict.waiting_reason() {
            None => CheckOutcome::Pass,
            Some(reason) => CheckOutcome::no_verdict(reason),
        };
        let result = CheckResult {
            check: APPROVAL_CHECK.to_string(),
            tree: run.tree().to_string(),
            stamp: format!("{} {}", part.patch_id, part.amend_patch_id)
                .trim()
                .to_string(),
            outcome,
        };
        self.store_result(writer, &row.change_id, &result)?;
        Ok((verdict, result))
    }
}

/// The first eight hex digits of `commit`, as a person reads it.
pub(crate) fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

/// The log line of an approved verdict.
pub(crate) fn approved_line(verdict: &ApprovalVerdict) -> Option<String> {
    match verdict {
        ApprovalVerdict::Approved { line } => Some(line.clone()),
        ApprovalVerdict::NeedsPerson { .. } => None,
    }
}
