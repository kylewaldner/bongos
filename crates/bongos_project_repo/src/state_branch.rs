//! The state branch: snapshots committed without a checkout (blobs from `git hash-object -w`, a
//! temporary index `state/snapshot.index`, `write-tree`, `commit-tree`, a compare-and-swap of the
//! ref), and the thinning of old snapshots to the last of each day. No working copy and no
//! commit of master ever sees these files: the branch has no `definitions/` and is never merged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};

use crate::create::MAIN_BRANCH;
use crate::git::{Git, Identity};

/// The temporary index a snapshot is built in.
pub const SNAPSHOT_INDEX_FILE: &str = "snapshot.index";
/// The folder the files of a snapshot are written to on their way into git.
const SNAPSHOT_FILES_DIR: &str = "snapshot.files";
/// The line of a snapshot's message that says when it was taken.
const AT_PREFIX: &str = "at: ";

/// The full ref of the state branch `name`, which must be a plain branch name that is neither
/// master nor under it.
pub fn state_ref(name: &str) -> Result<String> {
    let refused = |why: &str| -> Result<String> {
        bail!("the state branch {name:?} {why}; set project_repository.state_branch")
    };
    if name.is_empty() || name.len() > 200 {
        return refused("is empty or too long");
    }
    if name.starts_with("refs/") || name == "HEAD" || name.starts_with('-') {
        return refused("must be a branch name without refs/heads/");
    }
    let lower = name.to_ascii_lowercase();
    for protected in [MAIN_BRANCH, "master"] {
        if lower == protected || lower.starts_with(&format!("{protected}/")) {
            return refused("would be master");
        }
    }
    let valid_characters = name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character));
    let valid_components = name.split('/').all(|component| {
        !component.is_empty()
            && !component.starts_with('.')
            && !component.ends_with(".lock")
            && !component.ends_with('.')
    });
    if !valid_characters || !valid_components || name.contains("..") || name.contains("@{") {
        return refused("is not a valid branch name");
    }
    Ok(format!("refs/heads/{name}"))
}

/// How a snapshot commit ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Committed {
    /// The branch moved to `commit`.
    Moved { commit: String },
    /// The files are those of `commit`, the branch's tip; nothing was committed.
    Unchanged { commit: String },
    /// The branch was no longer `previous`; nothing changed.
    LostRace,
}

/// Removes the temporary index and files when dropped, however the snapshot ended.
struct Scratch {
    index: PathBuf,
    files: PathBuf,
}

impl Scratch {
    fn new(state: &Path) -> Result<Self> {
        let scratch = Self {
            index: state.join(SNAPSHOT_INDEX_FILE),
            files: state.join(SNAPSHOT_FILES_DIR),
        };
        scratch.clean();
        std::fs::create_dir_all(&scratch.files)
            .with_context(|| format!("creating {}", scratch.files.display()))?;
        Ok(scratch)
    }

    fn clean(&self) {
        std::fs::remove_file(&self.index).ok();
        std::fs::remove_file(self.index.with_extension("index.lock")).ok();
        std::fs::remove_dir_all(&self.files).ok();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.clean();
    }
}

/// The message of a snapshot taken at `at` from `generation`, for `reason`.
pub(crate) fn snapshot_message(reason: &str, at: DateTime<Utc>, generation: u64) -> String {
    format!(
        "snapshot ({reason})\n\n{AT_PREFIX}{}\ngeneration: {generation}\n",
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// When the snapshot with `message` was taken.
pub(crate) fn snapshot_time(message: &str) -> Option<DateTime<Utc>> {
    message
        .lines()
        .find_map(|line| line.strip_prefix(AT_PREFIX))
        .and_then(|text| DateTime::parse_from_rfc3339(text.trim()).ok())
        .map(|time| time.with_timezone(&Utc))
}

/// Commits `files` on `reference` of the repository at `repository`, with `previous` (the tip
/// read before) as parent, using `state` for the temporary index. Only moves the ref while it is
/// still `previous`.
pub(crate) async fn commit_snapshot(
    git: &Git,
    repository: &Path,
    state: &Path,
    reference: &str,
    files: &BTreeMap<String, String>,
    previous: Option<&str>,
    message: &str,
) -> Result<Committed> {
    let scratch = Scratch::new(state)?;
    let mut paths = Vec::with_capacity(files.len());
    for (number, text) in files.values().enumerate() {
        let path = scratch.files.join(number.to_string());
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        paths.push(path);
    }
    let blobs = git.hash_objects(repository, &paths).await?;
    if blobs.len() != files.len() {
        bail!(
            "git hash-object gave {} ids for {} files",
            blobs.len(),
            files.len()
        );
    }
    let mut entries = Vec::new();
    for (path, blob) in files.keys().zip(&blobs) {
        entries.extend_from_slice(format!("100644 blob {blob}\t{path}").as_bytes());
        entries.push(0);
    }
    git.run_on_index(
        repository,
        &scratch.index,
        &["update-index", "-z", "--add", "--index-info"],
        Some(entries),
    )
    .await?;
    let tree = git
        .run_on_index(repository, &scratch.index, &["write-tree"], None)
        .await?;
    let tree = String::from_utf8_lossy(&tree).trim().to_string();
    drop(scratch);
    if let Some(previous) = previous
        && git.tree_of(repository, previous).await? == tree
    {
        return Ok(Committed::Unchanged {
            commit: previous.to_string(),
        });
    }
    let parents: Vec<&str> = previous.into_iter().collect();
    let commit = git
        .commit_tree(repository, &tree, &parents, message, &Identity::default())
        .await?;
    match git
        .update_ref_cas(repository, reference, &commit, previous.unwrap_or(""))
        .await
    {
        Ok(()) => Ok(Committed::Moved { commit }),
        Err(error) => {
            let now = git.rev_parse(repository, reference).await.ok();
            if now.as_deref() != previous {
                Ok(Committed::LostRace)
            } else {
                Err(error.into())
            }
        }
    }
}

/// The tip of `reference`, `None` when the branch does not exist yet.
pub(crate) async fn tip(git: &Git, repository: &Path, reference: &str) -> Option<String> {
    git.rev_parse(repository, reference).await.ok()
}

/// One snapshot on the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotCommit {
    pub(crate) commit: String,
    pub(crate) tree: String,
    pub(crate) message: String,
}

/// The snapshots of `reference`, oldest first (its first-parent chain).
pub(crate) async fn snapshots(
    git: &Git,
    repository: &Path,
    reference: &str,
) -> Result<Vec<SnapshotCommit>> {
    let log = git
        .run(
            repository,
            &[
                "log",
                "--first-parent",
                "--format=%H%x1f%T%x1f%B%x1e",
                reference,
            ],
        )
        .await?;
    let mut commits: Vec<SnapshotCommit> = log
        .split('\u{1e}')
        .filter_map(|record| {
            let mut parts = record.trim_start_matches('\n').splitn(3, '\u{1f}');
            let commit = parts.next()?.trim().to_string();
            let tree = parts.next()?.trim().to_string();
            let message = parts.next().unwrap_or_default().trim_end().to_string();
            (!commit.is_empty()).then_some(SnapshotCommit {
                commit,
                tree,
                message,
            })
        })
        .collect();
    commits.reverse();
    Ok(commits)
}

/// Which of `commits` (oldest first) thinning keeps: every snapshot taken at or after `cutoff`,
/// every snapshot whose time cannot be read, and the last of each day before it.
pub(crate) fn kept(commits: &[SnapshotCommit], cutoff: DateTime<Utc>) -> Vec<bool> {
    let days: Vec<Option<NaiveDate>> = commits
        .iter()
        .map(|commit| {
            snapshot_time(&commit.message)
                .filter(|time| *time < cutoff)
                .map(|time| time.date_naive())
        })
        .collect();
    days.iter()
        .enumerate()
        .map(|(index, day)| match day {
            None => true,
            Some(day) => days.get(index + 1).and_then(|next| *next) != Some(*day),
        })
        .collect()
}

/// Thins `reference` to the last snapshot of each day before `cutoff`, rebuilding the branch
/// from the first dropped snapshot with the same trees and messages and new parents, and moves
/// it through a compare-and-swap. Returns the old and new tips when it rewrote anything; a
/// branch that moved meanwhile is left for the next time.
pub(crate) async fn thin(
    git: &Git,
    repository: &Path,
    reference: &str,
    cutoff: DateTime<Utc>,
) -> Result<Option<(String, String)>> {
    let Some(old_tip) = tip(git, repository, reference).await else {
        return Ok(None);
    };
    let commits = snapshots(git, repository, reference).await?;
    let keep = kept(&commits, cutoff);
    let Some(first_dropped) = keep.iter().position(|kept| !kept) else {
        return Ok(None);
    };
    let mut parent = first_dropped
        .checked_sub(1)
        .and_then(|index| commits.get(index))
        .map(|commit| commit.commit.clone());
    for (commit, kept) in commits.iter().zip(&keep).skip(first_dropped) {
        if !kept {
            continue;
        }
        let parents: Vec<&str> = parent.as_deref().into_iter().collect();
        parent = Some(
            git.commit_tree(
                repository,
                &commit.tree,
                &parents,
                &commit.message,
                &Identity::default(),
            )
            .await?,
        );
    }
    let Some(new_tip) = parent else {
        return Ok(None);
    };
    match git
        .update_ref_cas(repository, reference, &new_tip, &old_tip)
        .await
    {
        Ok(()) => Ok(Some((old_tip, new_tip))),
        Err(error) => {
            if tip(git, repository, reference).await.as_deref() != Some(old_tip.as_str()) {
                log::info!("the state branch moved while it was thinned; thinning later");
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }
}

/// How many snapshots on `reference` the remote does not have: those not reachable from
/// `pushed`, or all of them when nothing was pushed.
pub(crate) async fn unpushed(
    git: &Git,
    repository: &Path,
    reference: &str,
    pushed: Option<&str>,
) -> Result<u64> {
    if tip(git, repository, reference).await.is_none() {
        return Ok(0);
    }
    let pushed = match pushed {
        Some(pushed)
            if git
                .rev_parse(repository, &format!("{pushed}^{{commit}}"))
                .await
                .is_ok() =>
        {
            Some(pushed)
        }
        _ => None,
    };
    let count = match pushed {
        Some(pushed) => {
            let excluded = format!("^{pushed}");
            git.run(repository, &["rev-list", "--count", reference, &excluded])
                .await?
        }
        None => {
            git.run(repository, &["rev-list", "--count", reference])
                .await?
        }
    };
    count
        .trim()
        .parse()
        .with_context(|| format!("reading the count {count:?}"))
}
