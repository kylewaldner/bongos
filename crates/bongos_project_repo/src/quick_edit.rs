//! Quick edits: a change to definitions made from a view, a chat or a step, with no working copy
//! checked out. The router's definition ops are written into a temporary index read from the
//! commit the command read, committed with `commit-tree`, and the branch `<prefix><name>` points
//! at that commit. The change's row, its staged state ops and its record (the tasks it marks
//! pending) are written in one state transaction; the landing then runs as for any change. A
//! person at the keyboard writes a `PersonEdit` approval, so the change lands with no further
//! action; anyone else's waits for `project: approve`. A refused landing drops the staged state,
//! and a change that did not land is checked out as a working copy for the person to repair.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::checks::ApprovalBasis;
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_project_repo_model::{
    ChangeMeta, DefinitionTree, PersonAtKeyboard, RelPath, split, strip_trailers,
};
use bongos_work_model::TaskId;
use bongos_work_model::commands::{Change, Write};
use bongos_work_model::slug::MAX_SLUG_LEN;
use bongos_work_model::store::{PendingMark, WorkTree};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::approval::PersonGrant;
use crate::changes::{ChangeRow, ChangeState};
use crate::create::ProjectRepo;
use crate::landing::{ALREADY_LANDING, LandMode, LandRequest, LandingResult, author_identity};
use crate::router::{
    DefinitionOp, FileOp, check_file_op, project_of, staged_file_op, staged_file_ops,
};
use crate::state::StateRead;
use crate::state_ops::StateOp;
use crate::working_copies::{clean_name, fresh_name};

/// The `kv` namespace of the quick edits' records, by change id.
pub const QUICK_EDITS_NAMESPACE: &str = "quick-edits";
/// The mark of a task whose quick edit is landing or waits to.
pub const LANDING_MARK: &str = "landing";
/// The mark of a task whose quick edit was refused or stopped.
pub const NOT_LANDED_MARK: &str = "not landed";
/// Why a write computed on an older snapshot of a task's definition is refused.
pub const STALE_SNAPSHOT: &str = "changed since it was read; try again";

/// A quick edit as the router computed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickEdit {
    /// The commit whose definitions the command read: the branch starts there, and the landing
    /// rebases it onto the verified commit.
    pub base: String,
    pub definition: Vec<DefinitionOp>,
    pub state: Vec<StateOp>,
    pub files: Vec<FileOp>,
    pub meta: ChangeMeta,
    /// The tasks whose rows show the change as pending.
    pub tasks: Vec<TaskId>,
    /// The name asked for the change's branch and row (cleaned, and numbered when taken); a
    /// generated one when `None`.
    #[serde(default)]
    pub name: Option<String>,
}

/// A quick edit whose branch and row exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickEditStarted {
    pub change_id: String,
    pub name: String,
    pub branch: String,
    pub commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QuickEditRecord {
    subject: String,
    tasks: Vec<TaskId>,
    label: String,
}

/// The tasks a change writes (a task file of each), for the pending marks.
pub fn written_tasks(change: &Change) -> Vec<TaskId> {
    let mut tasks: Vec<TaskId> = change
        .writes
        .iter()
        .filter_map(|write| match write {
            Write::Task { file, .. } => Some(file.id.clone()),
            _ => None,
        })
        .collect();
    tasks.dedup();
    tasks
}

/// Refuses `change`, computed on `tree`, when a definition it writes is no longer the one in
/// `definitions` (the commit the quick edit starts from): the command read an older snapshot,
/// and writing the whole definition would undo the newer one.
pub fn check_fresh(
    tree: &WorkTree,
    change: &Change,
    definitions: &DefinitionTree,
) -> Result<(), String> {
    for write in &change.writes {
        let file = match write {
            Write::Task { file, .. } => file,
            // The router writes the whole project file when the change alters it: refused when
            // the one read is not the one at the commit the quick edit starts from.
            Write::Project { dir, file } => {
                let read = project_of(tree, dir);
                if read != Some(file) && read != definitions.project.as_ref() {
                    return Err(format!("the project {STALE_SNAPSHOT}"));
                }
                continue;
            }
            _ => continue,
        };
        let Some(task) = tree.task_by_id(&file.id) else {
            continue;
        };
        let (Some(folder), Some(read)) = (task.definition.as_ref(), task.file.as_ref()) else {
            continue;
        };
        let fresh = definitions.node(&file.id).is_some_and(|node| {
            Path::new(&node.folder) == folder.as_path() && node.definition == split(read).0
        });
        if !fresh {
            return Err(format!("{} {STALE_SNAPSHOT}", task.named_goal()));
        }
    }
    Ok(())
}

/// Refuses `base` unless it is a full commit id (40 or 64 hex digits): a quick edit from
/// another process names it, and git must never read it as an option or a revision expression.
pub(crate) fn check_commit_id(base: &str) -> Result<()> {
    if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{base:?} is not a commit id");
    }
    Ok(())
}

/// Sets the pending mark of every task an open quick edit marks.
pub(crate) fn mark_pending(tree: &mut WorkTree, read: &StateRead) -> Result<()> {
    let mut marks: Vec<(TaskId, PendingMark)> = Vec::new();
    for row in read.changes()? {
        if !matches!(row.state, ChangeState::Open | ChangeState::Requested) {
            continue;
        }
        let Some(value) = read.kv(QUICK_EDITS_NAMESPACE).get(&row.change_id)? else {
            continue;
        };
        let Ok(record) = value.try_into::<QuickEditRecord>() else {
            continue;
        };
        for task in record.tasks {
            marks.push((
                task,
                PendingMark {
                    change_id: row.change_id.clone(),
                    label: record.label.clone(),
                },
            ));
        }
    }
    if marks.is_empty() {
        return Ok(());
    }
    for project in &mut tree.projects {
        project.task.walk_mut(&mut |task| {
            if let Some(id) = task.id()
                && let Some((_, mark)) = marks.iter().rev().find(|(task_id, _)| task_id == id)
            {
                task.pending = Some(mark.clone());
            }
        });
    }
    Ok(())
}

fn index_line(mode: &str, object: &str, path: &str) -> Vec<u8> {
    let mut line = format!("{mode} {object}\t{path}").into_bytes();
    line.push(0);
    line
}

fn definitions_path(path: &str) -> String {
    format!("{DEFINITIONS_DIR}/{path}")
}

impl PersonGrant {
    /// The grant of a person at the keyboard whose marker the write carried.
    pub(crate) fn from_marker(
        author: &bongos_project_repo_model::Author,
        _marker: PersonAtKeyboard,
    ) -> Option<Self> {
        (author.kind() == bongos_project_repo_model::AuthorKind::Person)
            .then(|| Self::for_verified_peer(author.name.clone()))
    }
}

impl ProjectRepo {
    /// Makes the branch, the row and the staged state of `edit`, and its `PersonEdit` approval
    /// when `attestation` is the marker of the person who wrote it. Does not land it; see
    /// [`Self::land_quick_edit`].
    pub async fn quick_edit(
        &self,
        edit: QuickEdit,
        attestation: Option<PersonAtKeyboard>,
        now: DateTime<Utc>,
    ) -> Result<QuickEditStarted> {
        let grant =
            attestation.and_then(|marker| PersonGrant::from_marker(&edit.meta.author, marker));
        self.start_quick_edit(edit, grant, now).await
    }

    pub(crate) async fn start_quick_edit(
        &self,
        edit: QuickEdit,
        grant: Option<PersonGrant>,
        now: DateTime<Utc>,
    ) -> Result<QuickEditStarted> {
        let writer = self.writer_or_error()?;
        if edit.definition.is_empty() {
            bail!("a quick edit needs a change to definitions");
        }
        check_commit_id(&edit.base)?;
        // Every file op (a move, a new task folder, a written file) waits for the landing, with
        // the state: a change that does not land makes none. Each is checked here, and again at
        // the landing, so none leaves the files root, whoever sent it.
        let mut state = edit.state.clone();
        state.extend(edit.files.iter().map(staged_file_op));
        for op in staged_file_ops(&state) {
            check_file_op(&self.location.files, &op)?;
        }
        let repository = self.location.repository.clone();
        let prefix = self.land_options().branch_prefix;
        let wanted = edit.name.as_deref().and_then(|name| clean_name(name).ok());
        let mut name = wanted.clone().unwrap_or_else(fresh_name);
        let mut attempt = 1;
        while self
            .git
            .branch_exists(&repository, &format!("{prefix}{name}"))
            .await
            || self.working_copy_path(&name).exists()
        {
            attempt += 1;
            name = match &wanted {
                // Cleaned again, the stem shortened first so the suffix survives the length
                // limit: a name that cleaned to the same text every time would never be free.
                Some(wanted) => {
                    let suffix = format!("-{attempt}");
                    let stem: String = wanted
                        .chars()
                        .take(MAX_SLUG_LEN.saturating_sub(suffix.len()))
                        .collect();
                    clean_name(&format!("{stem}{suffix}"))?
                }
                None => fresh_name(),
            };
        }
        let branch = format!("{prefix}{name}");
        let scratch = self.location.state.join("quick-edits").join(&name);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("creating {}", scratch.display()))?;
        let tree = self
            .quick_edit_tree(&edit.base, &edit.definition, &scratch)
            .await;
        std::fs::remove_dir_all(&scratch).ok();
        let tree = tree?;
        let subject = strip_trailers(&edit.meta.subject);
        let message = if subject.trim().is_empty() {
            format!("Quick edit {name}")
        } else {
            subject.clone()
        };
        let identity = author_identity(&edit.meta.author);
        let commit = self
            .git
            .commit_tree(&repository, &tree, &[&edit.base], &message, &identity)
            .await?;
        // Created only while no branch of that name exists.
        self.git
            .run(
                &repository,
                &["update-ref", &format!("refs/heads/{branch}"), &commit, ""],
            )
            .await?;

        let change_id = bongos_work_model::new_id();
        let row = ChangeRow {
            change_id: change_id.clone(),
            name: name.clone(),
            branch: branch.clone(),
            author: edit.meta.author.clone(),
            created: now,
            updated: now,
            state: ChangeState::Open,
            commit: None,
            amend_commits: Vec::new(),
        };
        let record = QuickEditRecord {
            subject: edit.meta.subject.clone(),
            tasks: edit.tasks.clone(),
            label: LANDING_MARK.to_string(),
        };
        let record = toml::Value::try_from(&record).context("serializing a quick edit")?;
        // Everything that can fail runs before the one transaction that writes the row, the
        // staged state, the record and the approval: a failure leaves nothing open.
        let prepared = match grant {
            Some(grant) => self
                .person_approval(&row, ApprovalBasis::PersonEdit, grant, now)
                .await
                .map(Some),
            None => Ok(None),
        };
        let written = prepared.and_then(|approval| {
            writer.transact(|tx| {
                tx.set_change(&row)?;
                if !state.is_empty() {
                    tx.stage(&change_id, &state)?;
                }
                tx.kv(QUICK_EDITS_NAMESPACE)
                    .put(&change_id, record.clone())?;
                if let Some((text, _)) = &approval {
                    tx.add_approval(&change_id, text)?;
                }
                Ok(())
            })?;
            Ok(approval)
        });
        match written {
            Ok(Some((_, approval))) => self.approval_recorded(&approval, &row),
            Ok(None) => {}
            Err(error) => {
                self.git.branch_delete(&repository, &branch).await.ok();
                return Err(error);
            }
        }
        Ok(QuickEditStarted {
            change_id,
            name,
            branch,
            commit,
        })
    }

    /// The tree of `base` with `ops` made in `definitions/`, built in a temporary index in
    /// `scratch`: a constant number of git processes, however many files change.
    async fn quick_edit_tree(
        &self,
        base: &str,
        ops: &[DefinitionOp],
        scratch: &Path,
    ) -> Result<String> {
        let repository = &self.location.repository;
        let index = scratch.join("index");
        // `base` is a checked commit id, so the null id of its hash has its length.
        let zero = "0".repeat(base.len());
        self.git
            .run_on_index(repository, &index, &["read-tree", base], None)
            .await?;
        // The moves first, each against the index the earlier ones left: the writes name the
        // paths after them.
        for op in ops {
            let DefinitionOp::Move { from, to } = op else {
                continue;
            };
            for folder in [from, to] {
                RelPath::new(folder.clone())
                    .ok_or_else(|| anyhow!("{folder} is not a definitions folder"))?;
            }
            let from_prefix = format!("{}/", definitions_path(from));
            let listed = self
                .git
                .run_on_index(
                    repository,
                    &index,
                    &["ls-files", "-s", "-z", "--", &from_prefix],
                    None,
                )
                .await?;
            let mut info = Vec::new();
            for entry in listed
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                let text = std::str::from_utf8(entry).context("reading the index")?;
                let (meta, path) = text
                    .split_once('\t')
                    .ok_or_else(|| anyhow!("reading the index entry {text}"))?;
                let mut parts = meta.split(' ');
                let (Some(mode), Some(object)) = (parts.next(), parts.next()) else {
                    bail!("reading the index entry {text}");
                };
                let rest = path
                    .strip_prefix(&from_prefix)
                    .ok_or_else(|| anyhow!("{path} is not under {from_prefix}"))?;
                info.extend(index_line("0", &zero, path));
                info.extend(index_line(
                    mode,
                    object,
                    &format!("{}/{rest}", definitions_path(to)),
                ));
            }
            if info.is_empty() {
                bail!("there is no task folder {from} to move");
            }
            self.git
                .run_on_index(
                    repository,
                    &index,
                    &["update-index", "-z", "--index-info"],
                    Some(info),
                )
                .await?;
        }
        let mut info = Vec::new();
        let mut blobs: Vec<PathBuf> = Vec::new();
        let mut targets: Vec<&RelPath> = Vec::new();
        for (number, op) in ops.iter().enumerate() {
            match op {
                DefinitionOp::Write { path, bytes } => {
                    let blob = scratch.join(format!("blob-{number}"));
                    std::fs::write(&blob, bytes)
                        .with_context(|| format!("writing {}", blob.display()))?;
                    blobs.push(blob);
                    targets.push(path);
                }
                DefinitionOp::Delete { path } => {
                    info.extend(index_line("0", &zero, &definitions_path(path.as_str())));
                }
                DefinitionOp::Move { .. } => {}
            }
        }
        let objects = self.git.hash_objects(repository, &blobs).await?;
        if objects.len() != targets.len() {
            bail!("git wrote {} of {} files", objects.len(), targets.len());
        }
        for (object, path) in objects.iter().zip(targets) {
            info.extend(index_line(
                "100644",
                object,
                &definitions_path(path.as_str()),
            ));
        }
        if !info.is_empty() {
            self.git
                .run_on_index(
                    repository,
                    &index,
                    &["update-index", "-z", "--index-info"],
                    Some(info),
                )
                .await?;
        }
        let tree = self
            .git
            .run_on_index(repository, &index, &["write-tree"], None)
            .await?;
        Ok(String::from_utf8_lossy(&tree).trim().to_string())
    }

    /// Lands the quick edit `change_id`. A refusal drops its staged state; a change that did not
    /// land is marked "not landed" and stays checked out for the person to repair.
    pub async fn land_quick_edit(
        &self,
        change_id: &str,
        now: DateTime<Utc>,
    ) -> Result<LandingResult> {
        let request = (|| {
            let row = self
                .change(change_id)?
                .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
            let record = self.quick_edit_record(change_id)?;
            let subject = record.map(|record| record.subject).unwrap_or_default();
            anyhow::Ok(LandRequest {
                meta: ChangeMeta::new(subject, row.author.clone()),
                mode: LandMode::Land,
            })
        })();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                // The landing never started, so no landing reports its end.
                self.engine.emit(
                    change_id,
                    crate::LandingEvent::Ended {
                        reason: format!("the landing failed: {error:#}"),
                    },
                );
                return Err(error);
            }
        };
        let result = self.land_now(change_id, request, now).await?;
        let not_landed = match &result {
            LandingResult::Refused { .. } => {
                let writer = self.writer_or_error()?;
                writer.transact(|tx| {
                    if tx.staged(change_id)?.is_some() {
                        tx.drop_staged(change_id)?;
                    }
                    Ok(())
                })?;
                true
            }
            LandingResult::Conflict { .. } => true,
            LandingResult::NoVerdict { reason } => reason != ALREADY_LANDING,
            _ => false,
        };
        if not_landed {
            self.relabel_quick_edit(change_id, NOT_LANDED_MARK)?;
            self.check_out_change(change_id).await?;
        }
        Ok(result)
    }

    fn quick_edit_record(&self, change_id: &str) -> Result<Option<QuickEditRecord>> {
        let value = self.read_state(|read| read.kv(QUICK_EDITS_NAMESPACE).get(change_id))?;
        value
            .map(|value| value.try_into().context("reading a quick edit"))
            .transpose()
    }

    /// The subject and the mark (`landing` or `not landed`) of the quick edit `change_id`, when
    /// it is one.
    pub(crate) fn quick_edit_label(&self, change_id: &str) -> Option<(String, String)> {
        let record = self.quick_edit_record(change_id).ok().flatten()?;
        Some((record.subject, record.label))
    }

    fn relabel_quick_edit(&self, change_id: &str, label: &str) -> Result<()> {
        let Some(mut record) = self.quick_edit_record(change_id)? else {
            return Ok(());
        };
        record.label = label.to_string();
        let value = toml::Value::try_from(&record).context("serializing a quick edit")?;
        self.writer_or_error()?
            .transact(|tx| tx.kv(QUICK_EDITS_NAMESPACE).put(change_id, value.clone()))
    }

    /// Checks out the working copy of `change_id` when only its branch exists (a quick edit,
    /// or `o` in the landing view), and returns where it is.
    pub async fn check_out_change(&self, change_id: &str) -> Result<PathBuf> {
        let row = self
            .change(change_id)?
            .ok_or_else(|| anyhow!("there is no change {change_id}"))?;
        let path = self.working_copy_path(&row.name);
        if path.is_dir() {
            return Ok(path);
        }
        if !matches!(row.state, ChangeState::Open | ChangeState::Requested) {
            bail!("change {change_id} is no longer open");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        self.git
            .worktree_checkout(&self.location.repository, &path, &row.branch)
            .await?;
        Ok(path)
    }
}
