//! Moving a task subtree from one project to another: one [`MoveCoordinator`] per move, whose
//! rows live in the source's state store under the move id (and follow in the target's), so
//! every step is idempotent by the move id and a coordinator rebuilt from the rows after a
//! restart resumes where the last one stopped.
//!
//! Confirming the preview holds the source's tasks (claims written before any working copy).
//! Then two working copies: the removal in the source and the addition in the target (folders
//! and documents copied byte for byte, crossing edges dropped, missing repositories added to the
//! target's `project.toml`). Both land through their own checks, approval and queue; the source's
//! landing waits until the target's landed. The target's step 8 imports the moved rows and
//! claims them, the source's step 8 deletes its own, and the coordinator then clears the target's
//! claims. A source refused after the target landed has the target rolled back by a revert the
//! coordinator makes and records, which its approval rule covers; a rollback that fails leaves
//! the claims and asks for a person.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::checks::state_is_started;
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE, TASK_FILE};
use bongos_project_repo_model::moves::{MovePlan, MoveRefusal, MoveRequest, move_plan};
use bongos_project_repo_model::{Author, ChangeMeta, RelPath, TaskDefinition};
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::{ProjectFile, new_id, serialize_project};
use chrono::{DateTime, Utc};
use futures::FutureExt as _;
use futures::future::Either;
use serde::Serialize;

use crate::approval::PersonGrant;
use crate::changes::ChangeFilter;
use crate::create::ProjectRepo;
use crate::git::{CommitId, TreeRef};
use crate::landing::{LandMode, LandRequest, LandingRecord, LandingResult, author_identity};
use crate::loader::load_definition_tree;
use crate::moves::{
    MovePhase, MoveRecord, MoveRole, claim, holding_dir, read_record, release_claims, write_record,
};

/// What a move ended as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    /// Both sides landed and every claim is cleared.
    Moved,
    /// Refused before the target landed; neither project changed.
    Refused { reasons: Vec<String> },
    /// The source was refused after the target landed, and the target was rolled back.
    RolledBack { reasons: Vec<String> },
    /// The rollback failed: the claims stay and both changes show "move <id> needs you".
    NeedsYou { reason: String },
}

/// The coordinator of one move.
pub struct MoveCoordinator {
    source: ProjectRepo,
    target: ProjectRepo,
    move_id: String,
    author: Author,
    /// The person who confirmed the move at a window, in this process only: each side's landing
    /// is that person's own change. A coordinator rebuilt after a restart has none, and a
    /// person's side waits for `project: approve`.
    person: Option<String>,
}

#[derive(Serialize)]
struct RollbackRecord<'a> {
    move_id: &'a str,
    target_commit: &'a str,
    target_approved: bool,
}

/// The reasons a landing result that is not a landing gives.
fn reasons_of(result: &LandingResult) -> Vec<String> {
    match result {
        LandingResult::Refused { reasons } => reasons.clone(),
        LandingResult::Conflict { files } => {
            vec![format!("conflicts in {}", files.join(", "))]
        }
        LandingResult::NoVerdict { reason } => vec![reason.clone()],
        LandingResult::Abandoned => vec!["the change was abandoned".to_string()],
        LandingResult::NotRequested => vec!["the change was not asked to land".to_string()],
        LandingResult::Checked { .. } => vec!["the change was only checked".to_string()],
        LandingResult::Landed { .. } => Vec::new(),
    }
}

async fn verified_tree(repo: &ProjectRepo) -> Result<bongos_project_repo_model::DefinitionTree> {
    let commit = repo.shown_commit().await?;
    load_definition_tree(
        &repo.git,
        &repo.location.repository,
        &TreeRef::Commit(commit),
    )
    .await
}

/// `definitions/<folder>/task.toml` in the checkout at `root`.
fn task_file(root: &Path, folder: &str) -> Result<PathBuf> {
    let relative = RelPath::new(format!("{folder}/{TASK_FILE}"))
        .ok_or_else(|| anyhow!("{folder} is not a task folder"))?;
    Ok(root.join(DEFINITIONS_DIR).join(relative.as_str()))
}

/// Drops the edges of the task file at `path` that `dropped` names (holder `task`).
fn drop_edges(path: &Path, plan: &MovePlan, holder_moves: bool) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut definition: TaskDefinition =
        toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let dropped: Vec<_> = plan
        .dropped_edges
        .iter()
        .filter(|edge| edge.holder_moves == holder_moves && edge.task == definition.id)
        .collect();
    if dropped.is_empty() {
        return Ok(());
    }
    use bongos_project_repo_model::EdgeKind;
    definition.after.retain(|edge| {
        !dropped
            .iter()
            .any(|dropped| dropped.kind == EdgeKind::After && dropped.target == edge.task)
    });
    if definition.stacked_on.as_ref().is_some_and(|edge| {
        dropped
            .iter()
            .any(|dropped| dropped.kind == EdgeKind::StackedOn && dropped.target == edge.task)
    }) {
        definition.stacked_on = None;
    }
    let text = toml::to_string(&definition).context("writing a task definition")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Copies the regular files and folders under `from` to `to`; a symbolic link is left out.
fn copy_folder(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_folder(&entry.path(), &destination)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            std::fs::write(&destination, bytes)
                .with_context(|| format!("writing {}", destination.display()))?;
        } else {
            log::info!(
                "a move leaves out {}: not a file or a folder",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn remove_folder(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

impl MoveCoordinator {
    /// The plan of moving `request.task_id` from `source` to `target`, as the preview shows it;
    /// changes nothing.
    pub async fn preview(
        source: &ProjectRepo,
        target: &ProjectRepo,
        request: &MoveRequest,
    ) -> Result<std::result::Result<MovePlan, MoveRefusal>> {
        let source_tree = verified_tree(source).await?;
        let target_tree = verified_tree(target).await?;
        let ids: Vec<String> = source_tree
            .nodes()
            .into_iter()
            .map(|node| node.definition.id.clone())
            .collect();
        let live = source.live_facts_for(&ids)?;
        Ok(move_plan(&source_tree, &target_tree, request, &live))
    }

    /// Confirms the move the preview of `request` shows, by `author`; `ctx` is the context of
    /// the person who confirmed it at a window, when one did (its attestation is taken, and
    /// `author` should be its author). The source's tasks are held from here, in the same
    /// transaction that checks again that none started and no other move holds one.
    pub async fn confirm(
        source: &ProjectRepo,
        target: &ProjectRepo,
        request: &MoveRequest,
        author: Author,
        ctx: Option<&mut WorkContext>,
    ) -> Result<std::result::Result<Self, MoveRefusal>> {
        let plan = match Self::preview(source, target, request).await? {
            Ok(plan) => plan,
            Err(refusal) => return Ok(Err(refusal)),
        };
        target.writer_or_error()?;
        let person = ctx
            .and_then(PersonGrant::from_context)
            .map(PersonGrant::into_name);
        let move_id = new_id();
        let record = MoveRecord {
            move_id: move_id.clone(),
            role: MoveRole::Source,
            phase: MovePhase::Confirmed,
            source_change: None,
            target_change: None,
            target_commit: None,
            rollback_change: None,
            problem: None,
            plan: plan.clone(),
            author: author.clone(),
            states: Default::default(),
        };
        let registry = source.check_registry();
        let writer = source.writer_or_error()?;
        let claimed = writer.transact(|tx| {
            let mut reasons = Vec::new();
            for task_id in plan.task_ids() {
                let started_row = tx.task(&task_id)?.as_ref().is_some_and(state_is_started);
                let started_live = registry
                    .live
                    .iter()
                    .any(|live| live.started(&task_id) || live.held(&task_id).is_some());
                if started_row || started_live {
                    reasons.push(format!("task {task_id} started; it cannot move"));
                }
            }
            if !reasons.is_empty() {
                return Ok(Err(MoveRefusal { reasons }));
            }
            for task_id in plan.task_ids() {
                if let Err(error) = claim(tx, &task_id, &move_id) {
                    return Ok(Err(MoveRefusal {
                        reasons: vec![format!("{error:#}")],
                    }));
                }
            }
            write_record(tx, &record)?;
            Ok(Ok(()))
        })?;
        if let Err(refusal) = claimed {
            return Ok(Err(refusal));
        }
        log::info!(
            "move {move_id}: {} from {} to {} confirmed",
            plan.slug,
            plan.source.title,
            plan.target.title
        );
        Ok(Ok(Self {
            source: source.clone(),
            target: target.clone(),
            move_id,
            author,
            person,
        }))
    }

    /// The coordinator of the move `move_id`, rebuilt from its rows in `source` (after a
    /// restart). `target` must be the project the rows name.
    pub fn resume(source: &ProjectRepo, target: &ProjectRepo, move_id: &str) -> Result<Self> {
        let record = source
            .read_state(|read| read_record(read, move_id))?
            .ok_or_else(|| anyhow!("there is no move {move_id}"))?;
        if record.role != MoveRole::Source {
            bail!("move {move_id} did not start in this project");
        }
        if target.location.id != record.plan.target.id {
            bail!(
                "move {move_id} goes to {}, not to this project",
                record.plan.target.title
            );
        }
        Ok(Self {
            source: source.clone(),
            target: target.clone(),
            move_id: move_id.to_string(),
            author: record.author,
            person: None,
        })
    }

    pub fn move_id(&self) -> &str {
        &self.move_id
    }

    /// The move's row in the source.
    pub fn record(&self) -> Result<MoveRecord> {
        self.source
            .read_state(|read| read_record(read, &self.move_id))?
            .ok_or_else(|| anyhow!("there is no move {}", self.move_id))
    }

    fn grant(&self) -> Option<PersonGrant> {
        // The person who confirmed the move asked for both of its changes.
        self.person.clone().map(PersonGrant::for_verified_peer)
    }

    fn update_source(&self, f: impl FnOnce(&mut MoveRecord)) -> Result<MoveRecord> {
        let writer = self.source.writer_or_error()?;
        let record = writer.transact(|tx| {
            let mut record = read_record(&tx.as_read(), &self.move_id)?
                .ok_or_else(|| anyhow!("there is no move {}", self.move_id))?;
            f(&mut record);
            write_record(tx, &record)?;
            Ok(record)
        })?;
        self.source.engine.signal.notify();
        Ok(record)
    }

    fn update_target(&self, f: impl FnOnce(&mut MoveRecord)) -> Result<()> {
        let writer = self.target.writer_or_error()?;
        writer.transact(|tx| {
            if let Some(mut record) = read_record(&tx.as_read(), &self.move_id)? {
                f(&mut record);
                write_record(tx, &record)?;
            }
            Ok(())
        })?;
        self.target.engine.signal.notify();
        Ok(())
    }

    /// Drives the move from wherever its rows say it is to its end.
    pub async fn run(&self, now: DateTime<Utc>) -> Result<MoveOutcome> {
        loop {
            let record = self.record()?;
            match record.phase {
                MovePhase::Confirmed => self.prepare(now).await?,
                MovePhase::Prepared => self.land_both(now).await?,
                MovePhase::TargetLanded => {
                    self.land_source(now).await?;
                }
                MovePhase::SourceLanded => self.finish()?,
                MovePhase::RollingBack => match record.rollback_change {
                    None => {
                        self.make_rollback(now).await?;
                    }
                    Some(_) => {
                        self.land_rollback(now).await?;
                    }
                },
                MovePhase::Done => return Ok(MoveOutcome::Moved),
                MovePhase::Refused => {
                    return Ok(MoveOutcome::Refused {
                        reasons: record.problem.into_iter().collect(),
                    });
                }
                MovePhase::RolledBack => {
                    return Ok(MoveOutcome::RolledBack {
                        reasons: record.problem.into_iter().collect(),
                    });
                }
                MovePhase::NeedsYou => {
                    return Ok(MoveOutcome::NeedsYou {
                        reason: record.problem.unwrap_or_default(),
                    });
                }
            }
        }
    }

    fn short(&self) -> &str {
        &self.move_id[self.move_id.len().saturating_sub(8)..]
    }

    /// Makes the working copy `name` in `repo`, abandoning one of that name a crashed
    /// coordinator left behind unrecorded.
    async fn fresh_copy(
        &self,
        repo: &ProjectRepo,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<crate::WorkingCopy> {
        for row in repo.changes(&ChangeFilter::default())? {
            if row.name == name {
                repo.abandon_now(&row.change_id, now)
                    .await
                    .map_err(|error| anyhow!("{error}"))?;
            }
        }
        let prefix = repo.land_options().branch_prefix;
        repo.create_working_copy(name, &prefix, &self.author, now)
            .await
    }

    /// Step 1: the two working copies, committed, and the target's row with the moved state.
    pub async fn prepare(&self, now: DateTime<Utc>) -> Result<()> {
        let record = self.record()?;
        if record.phase != MovePhase::Confirmed {
            return Ok(());
        }
        let plan = &record.plan;
        let identity = author_identity(&self.author);
        let source_commit = self.source.shown_commit().await?.0;
        let source_tree = load_definition_tree(
            &self.source.git,
            &self.source.location.repository,
            &TreeRef::Commit(CommitId(source_commit.clone())),
        )
        .await?;

        // The source's removal.
        let removal = self
            .fresh_copy(&self.source, &format!("move-{}-out", self.short()), now)
            .await?;
        for edge in plan.dropped_edges.iter().filter(|edge| !edge.holder_moves) {
            let folder = source_tree
                .node(&edge.task)
                .map(|node| node.folder.clone())
                .ok_or_else(|| anyhow!("task {} is no longer in the source", edge.task))?;
            drop_edges(&task_file(&removal.path, &folder)?, plan, false)?;
        }
        let root_from = RelPath::new(plan.root_from())
            .ok_or_else(|| anyhow!("{} is not a task folder", plan.root_from()))?;
        remove_folder(&removal.path.join(DEFINITIONS_DIR).join(root_from.as_str()))?;
        self.source
            .git
            .commit_if_dirty(&removal.path, &self.source_meta(plan).subject, &identity)
            .await?;

        // The target's addition: the subtree's files from the source's master, byte for byte.
        let addition = self
            .fresh_copy(&self.target, &format!("move-{}-in", self.short()), now)
            .await?;
        let repository = &self.source.location.repository;
        let prefix = format!("{DEFINITIONS_DIR}/{}/", plan.root_from());
        let tree = self.source.git.tree_of(repository, &source_commit).await?;
        let entries: Vec<_> = self
            .source
            .git
            .ls_tree_r(repository, &tree)
            .await?
            .into_iter()
            .filter(|entry| entry.path.starts_with(&prefix))
            .collect();
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.kind != "blob" || entry.mode == "120000")
        {
            bail!("{} is not a plain file; it cannot move", entry.path);
        }
        let objects: Vec<String> = entries.iter().map(|entry| entry.object.clone()).collect();
        let blobs = self.source.git.cat_file_batch(repository, &objects).await?;
        for (entry, bytes) in entries.iter().zip(blobs) {
            let bytes = bytes.ok_or_else(|| anyhow!("{} is missing", entry.path))?;
            let rest = &entry.path[prefix.len()..];
            let relative = RelPath::new(format!("{}/{rest}", plan.root_to()))
                .ok_or_else(|| anyhow!("{} is not a definitions path", entry.path))?;
            let path = addition.path.join(DEFINITIONS_DIR).join(relative.as_str());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            #[cfg(unix)]
            if entry.mode == "100755" {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        drop_edges(&task_file(&addition.path, plan.root_to())?, plan, true)?;
        if !plan.missing_repositories.is_empty() {
            let path = addition.path.join(DEFINITIONS_DIR).join(PROJECT_FILE);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut project: ProjectFile =
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
            for repository in &plan.missing_repositories {
                if !project.repositories.contains(repository) {
                    project.repositories.push(repository.clone());
                }
            }
            let text = serialize_project(&project).map_err(|error| anyhow!("{error}"))?;
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        self.target
            .git
            .commit_if_dirty(&addition.path, &self.target_meta(plan).subject, &identity)
            .await?;

        // The files folders, held in the target until it lands, and the moved state.
        let holding = self.target.location.files.join(holding_dir(&self.move_id));
        remove_folder(&holding)?;
        let mut states = std::collections::BTreeMap::new();
        for task_id in plan.task_ids() {
            let from = self.source.location.files.join(&task_id);
            if from.is_dir() {
                copy_folder(&from, &holding.join(&task_id))?;
            }
            if let Some(state) = self.source.read_state(|read| read.task(&task_id))? {
                states.insert(task_id, state);
            }
        }
        let mirror = MoveRecord {
            role: MoveRole::Target,
            phase: MovePhase::Prepared,
            source_change: Some(removal.change_id.clone()),
            target_change: Some(addition.change_id.clone()),
            states,
            ..record.clone()
        };
        self.target
            .writer_or_error()?
            .transact(|tx| write_record(tx, &mirror))?;
        self.update_source(|record| {
            record.source_change = Some(removal.change_id.clone());
            record.target_change = Some(addition.change_id.clone());
            record.phase = MovePhase::Prepared;
        })?;
        Ok(())
    }

    fn dropped_body(plan: &MovePlan, first: String) -> String {
        let mut body = first;
        if let Some(line) = plan.dropped_edges_line() {
            body.push('\n');
            body.push_str(&line);
        }
        if let Some(from) = &plan.renamed_from {
            body.push_str(&format!(
                "\nRenamed from {from}: the target has a task {from}."
            ));
        }
        body
    }

    fn source_meta(&self, plan: &MovePlan) -> ChangeMeta {
        let mut meta = ChangeMeta::new(
            format!("Move {} to {}", plan.slug, plan.target.title),
            self.author.clone(),
        );
        meta.body = Self::dropped_body(plan, format!("Moved to {}.", plan.target.title));
        meta
    }

    fn target_meta(&self, plan: &MovePlan) -> ChangeMeta {
        let mut meta = ChangeMeta::new(
            format!("Move {} from {}", plan.slug, plan.source.title),
            self.author.clone(),
        );
        meta.body = Self::dropped_body(plan, format!("Moved from {}.", plan.source.title));
        if !plan.missing_repositories.is_empty() {
            meta.body.push_str(&format!(
                "\nAdded repositories: {}",
                plan.missing_repositories.join(", ")
            ));
        }
        meta
    }

    fn change_ids(&self) -> Result<(String, String)> {
        let record = self.record()?;
        match (record.source_change, record.target_change) {
            (Some(source), Some(target)) => Ok((source, target)),
            _ => bail!("move {} has no working copies yet", self.move_id),
        }
    }

    /// Step 2 for the target: lands its addition and records the landing (the source's landing
    /// waits for it), or ends the move refused when it does not land.
    pub async fn land_target(&self, now: DateTime<Utc>) -> Result<LandingResult> {
        let (_, target_change) = self.change_ids()?;
        let request = LandRequest {
            meta: self.target_meta(&self.record()?.plan),
            mode: LandMode::Land,
        };
        let result = self
            .target
            .land_granted(&target_change, request, self.grant(), now)
            .await?;
        self.target_ended(&result, now).await?;
        Ok(result)
    }

    async fn target_ended(&self, result: &LandingResult, now: DateTime<Utc>) -> Result<()> {
        match result {
            LandingResult::Landed { commit } => {
                let commit = commit.clone();
                self.update_source(|record| {
                    if record.phase == MovePhase::Prepared {
                        record.phase = MovePhase::TargetLanded;
                        record.target_commit = Some(commit);
                    }
                })?;
            }
            other => {
                if self.record()?.phase == MovePhase::Prepared {
                    self.refuse(reasons_of(other), now).await?;
                }
            }
        }
        Ok(())
    }

    /// Ends a move refused before its target landed: both changes abandoned, the source's
    /// claims and the target's held files cleared.
    async fn refuse(&self, reasons: Vec<String>, now: DateTime<Utc>) -> Result<()> {
        let record = self.record()?;
        let problem = reasons.join("; ");
        log::info!("move {}: refused: {problem}", self.move_id);
        if let Some(target_change) = &record.target_change {
            self.target.abandon_now(target_change, now).await.ok();
        }
        if let Some(source_change) = &record.source_change {
            self.source.abandon_now(source_change, now).await.ok();
        }
        remove_folder(&self.target.location.files.join(holding_dir(&self.move_id)))?;
        self.update_target(|mirror| {
            mirror.phase = MovePhase::Refused;
            mirror.problem = Some(problem.clone());
        })?;
        let writer = self.source.writer_or_error()?;
        writer.transact(|tx| {
            release_claims(tx, &self.move_id)?;
            let mut record = read_record(&tx.as_read(), &self.move_id)?
                .ok_or_else(|| anyhow!("there is no move {}", self.move_id))?;
            record.phase = MovePhase::Refused;
            record.problem = Some(problem.clone());
            write_record(tx, &record)
        })?;
        self.source.engine.signal.notify();
        Ok(())
    }

    fn source_request(&self) -> Result<LandRequest> {
        Ok(LandRequest {
            meta: self.source_meta(&self.record()?.plan),
            mode: LandMode::Land,
        })
    }

    /// Step 2 for both: the two landings run side by side, the source's held until the
    /// target's landed. A source that ends first (refused before the target landed) abandons
    /// the target's change when it still can.
    async fn land_both(&self, now: DateTime<Utc>) -> Result<()> {
        let (source_change, target_change) = self.change_ids()?;
        let source_landing = self
            .source
            .land_granted(&source_change, self.source_request()?, self.grant(), now)
            .boxed_local();
        let target_landing = self.land_target(now).boxed_local();
        let source_result = match futures::future::select(target_landing, source_landing).await {
            Either::Left((target, source_landing)) => {
                target?;
                source_landing.await?
            }
            Either::Right((source, target_landing)) => {
                let source = source?;
                if !matches!(source, LandingResult::Landed { .. }) {
                    self.target.abandon_now(&target_change, now).await.ok();
                }
                target_landing.await?;
                source
            }
        };
        self.source_ended(&source_result, now).await
    }

    /// Step 2 for the source alone, once the target landed.
    pub async fn land_source(&self, now: DateTime<Utc>) -> Result<LandingResult> {
        let (source_change, _) = self.change_ids()?;
        let result = self
            .source
            .land_granted(&source_change, self.source_request()?, self.grant(), now)
            .await?;
        self.source_ended(&result, now).await?;
        Ok(result)
    }

    async fn source_ended(&self, result: &LandingResult, now: DateTime<Utc>) -> Result<()> {
        let record = self.record()?;
        match (result, record.phase) {
            (LandingResult::Landed { .. }, _) => {
                if record.phase == MovePhase::SourceLanded {
                    self.finish()?;
                }
            }
            (other, MovePhase::TargetLanded) => {
                let problem = reasons_of(other).join("; ");
                log::info!(
                    "move {}: {} refused it after {} landed: {problem}; rolling back",
                    self.move_id,
                    record.plan.source.title,
                    record.plan.target.title
                );
                if let Some(source_change) = &record.source_change {
                    self.source.abandon_now(source_change, now).await.ok();
                }
                self.update_source(|record| {
                    record.phase = MovePhase::RollingBack;
                    record.problem = Some(problem);
                })?;
            }
            (other, MovePhase::Prepared) => self.refuse(reasons_of(other), now).await?,
            _ => {}
        }
        Ok(())
    }

    /// After the source landed: the target's claims are cleared and the source's files folders
    /// and the held copies removed.
    fn finish(&self) -> Result<()> {
        let record = self.record()?;
        if record.phase != MovePhase::SourceLanded {
            return Ok(());
        }
        self.target.writer_or_error()?.transact(|tx| {
            release_claims(tx, &self.move_id)?;
            if let Some(mut mirror) = read_record(&tx.as_read(), &self.move_id)? {
                mirror.phase = MovePhase::Done;
                mirror.states.clear();
                write_record(tx, &mirror)?;
            }
            Ok(())
        })?;
        self.target.engine.signal.notify();
        for task_id in record.plan.task_ids() {
            remove_folder(&self.source.location.files.join(&task_id))?;
        }
        remove_folder(&self.target.location.files.join(holding_dir(&self.move_id)))?;
        self.update_source(|record| record.phase = MovePhase::Done)?;
        log::info!(
            "move {}: {} moved from {} to {}",
            self.move_id,
            record.plan.slug,
            record.plan.source.title,
            record.plan.target.title
        );
        Ok(())
    }

    /// Ends the move needing a person: the claims stay.
    fn needs_you(&self, reason: String) -> Result<()> {
        log::error!("move {} needs you: {reason}", self.move_id);
        self.update_target(|mirror| {
            mirror.phase = MovePhase::NeedsYou;
            mirror.problem = Some(reason.clone());
        })?;
        self.update_source(|record| {
            record.phase = MovePhase::NeedsYou;
            record.problem = Some(reason);
        })?;
        Ok(())
    }

    /// Step 4: makes the rollback of the target's landing, a `git revert --no-commit` of its
    /// commit in a working copy the coordinator makes, and records it as the move's rollback in
    /// both projects' rows and in the target's move rollbacks. `None` when it cannot be made
    /// (it conflicts with a later landing), and the move then needs a person.
    pub async fn make_rollback(&self, now: DateTime<Utc>) -> Result<Option<String>> {
        let record = self.record()?;
        if let Some(rollback) = record.rollback_change {
            return Ok(Some(rollback));
        }
        let target_commit = record
            .target_commit
            .clone()
            .ok_or_else(|| anyhow!("move {} has no target landing", self.move_id))?;
        let repository = &self.target.location.repository;
        let verified = self.target.shown_commit().await?.0;
        if self
            .target
            .git
            .revert_tree(repository, &target_commit, &verified)
            .await?
            .is_none()
        {
            self.needs_you(format!(
                "the rollback of {} conflicts with a later landing in {}",
                record.plan.slug, record.plan.target.title
            ))?;
            return Ok(None);
        }
        let copy = self
            .fresh_copy(&self.target, &format!("move-{}-back", self.short()), now)
            .await?;
        let identity = author_identity(&self.author);
        let made = async {
            self.target
                .git
                .run(&copy.path, &["revert", "--no-commit", &target_commit])
                .await?;
            self.target
                .git
                .commit_if_dirty(
                    &copy.path,
                    &format!("Roll back move {}", self.move_id),
                    &identity,
                )
                .await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = made {
            self.target.abandon_now(&copy.change_id, now).await.ok();
            self.needs_you(format!("the rollback could not be made: {error:#}"))?;
            return Ok(None);
        }
        let approved = self.target_approved(&record)?;
        let rollback = copy.change_id.clone();
        let entry = toml::Value::try_from(RollbackRecord {
            move_id: &self.move_id,
            target_commit: &target_commit,
            target_approved: approved,
        })
        .context("serializing a move rollback")?;
        self.target.writer_or_error()?.transact(|tx| {
            tx.move_rollbacks().put(&rollback, entry)?;
            if let Some(mut mirror) = read_record(&tx.as_read(), &self.move_id)? {
                mirror.rollback_change = Some(rollback.clone());
                mirror.phase = MovePhase::RollingBack;
                write_record(tx, &mirror)?;
            }
            Ok(())
        })?;
        self.update_source(|record| record.rollback_change = Some(rollback.clone()))?;
        Ok(Some(rollback))
    }

    /// Whether the target's landing of the move was approved, from its landing record.
    fn target_approved(&self, record: &MoveRecord) -> Result<bool> {
        let Some(target_change) = &record.target_change else {
            return Ok(false);
        };
        let landings = self.target.read_state(|read| read.landings())?;
        Ok(landings
            .iter()
            .find(|(id, _)| id == target_change)
            .and_then(|(_, text)| toml::from_str::<LandingRecord>(text).ok())
            .is_some_and(|landing| landing.approval.is_some()))
    }

    /// Lands the recorded rollback as the move's own author, with no person's grant: its
    /// approval is the move's. Once it landed the source's claims are cleared; a rollback that
    /// does not land leaves the move needing a person.
    pub async fn land_rollback(&self, now: DateTime<Utc>) -> Result<LandingResult> {
        let record = self.record()?;
        let rollback = record
            .rollback_change
            .clone()
            .ok_or_else(|| anyhow!("move {} has no rollback", self.move_id))?;
        let mut meta = ChangeMeta::new(
            format!(
                "Roll back move of {} from {}",
                record.plan.slug, record.plan.source.title
            ),
            self.author.clone(),
        );
        meta.body = format!(
            "{} refused the move: {}",
            record.plan.source.title,
            record.problem.clone().unwrap_or_default()
        );
        let request = LandRequest {
            meta,
            mode: LandMode::Land,
        };
        let result = self.target.land_now(&rollback, request, now).await?;
        match &result {
            LandingResult::Landed { .. } => {
                for task_id in record.plan.task_ids() {
                    remove_folder(&self.target.location.files.join(&task_id))?;
                }
                let writer = self.source.writer_or_error()?;
                writer.transact(|tx| {
                    release_claims(tx, &self.move_id)?;
                    let mut record = read_record(&tx.as_read(), &self.move_id)?
                        .ok_or_else(|| anyhow!("there is no move {}", self.move_id))?;
                    record.phase = MovePhase::RolledBack;
                    write_record(tx, &record)
                })?;
                self.source.engine.signal.notify();
                log::info!("move {}: rolled back", self.move_id);
            }
            other => {
                self.needs_you(format!(
                    "the rollback did not land: {}",
                    reasons_of(other).join("; ")
                ))?;
            }
        }
        Ok(result)
    }
}

impl ProjectRepo {
    /// The moves started in this project that have not ended (a move that needs a person
    /// included), for the coordinators to resume after a restart.
    pub fn moves_in_flight(&self) -> Result<Vec<String>> {
        Ok(self
            .read_state(crate::moves::records)?
            .into_iter()
            .filter(|record| record.role == MoveRole::Source && !record.phase.is_over())
            .map(|record| record.move_id)
            .collect())
    }

    /// The row of the move `move_id` in this project.
    pub fn move_record(&self, move_id: &str) -> Result<Option<MoveRecord>> {
        self.read_state(|read| read_record(read, move_id))
    }
}
