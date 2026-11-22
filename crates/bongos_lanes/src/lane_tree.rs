//! A lane's persistent tree at a job start, and the success merge done in it.
//!
//! At each start the tree is added when missing (`git worktree add --detach`, below its root
//! only), its uncommitted work is saved when the run that last used it did not succeed, and it
//! is reset: `git reset --hard`, `git clean -fd` (ignored files such as build output stay; never
//! `-x`) and `git checkout --force -B <run branch> <start>`. Before git writes anything, the path
//! is checked to be the lane's own resolved tree, and not a primary checkout, a task worktree or
//! another lane's location, both as written and with links resolved; then git itself must agree
//! that it is a linked worktree of the repository. Nothing here takes a lock, deletes a tree or
//! pushes anything.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_workflow::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use bongos_workflow::worktrees::{Git, GitError, short_id};

use crate::create::{create_below_existing, lexical};
use crate::locations::{LaneLocation, LaneLocations, Place, tree_name};
use crate::prepare::{MakeTree, TreeRequest};

/// Where a save that cannot go on the run branch is kept: `refs/bongos/saved/<lane>/<run id>`.
pub const SAVED_REFS: &str = "refs/bongos/saved";
/// The author and committer of a save commit.
pub const SAVE_NAME: &str = "bongos";
pub const SAVE_EMAIL: &str = "bongos@localhost";

/// How the run that last used a tree ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastEnd {
    Succeeded,
    Failed,
    Halted,
    Cancelled,
}

impl LastEnd {
    /// The word of a save commit's message, `None` for a run whose work is not saved.
    fn saved_as(self) -> Option<&'static str> {
        match self {
            LastEnd::Succeeded => None,
            LastEnd::Failed => Some("failed"),
            LastEnd::Halted => Some("halted"),
            LastEnd::Cancelled => Some("cancelled"),
        }
    }
}

/// The run that last used a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastRun {
    pub run_id: String,
    /// Its run branch in this tree.
    pub run_branch: String,
    pub end: LastEnd,
}

/// The places a lane never resets, cleans or merges in, besides its own tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Forbidden {
    /// The primary checkout of every repository, and the project's own repository.
    pub checkouts: Vec<PathBuf>,
    /// The worktree of every task that records one.
    pub task_worktrees: Vec<PathBuf>,
    /// Every location of every other lane.
    pub other_lanes: Vec<PathBuf>,
}

/// `path` with links resolved as far as it exists, the rest appended as written.
fn resolved(path: &Path) -> PathBuf {
    let path = lexical(path);
    let mut existing = path.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// Checks, before git runs, that `path` is the lane's own tree `own` and touches nothing
/// forbidden: it must be `own` as written, must not be a link itself, and with links resolved it
/// must not be, hold or sit inside a primary checkout, a task worktree or another lane's
/// location. Returns the path with links resolved.
pub fn check_own_tree(path: &Path, own: &Path, forbidden: &Forbidden) -> Result<PathBuf, String> {
    let refuse = |why: String| Err(format!("{} is not this lane's tree to reset: {why}", path.display()));
    if !path.is_absolute() {
        return refuse("it is not an absolute path".to_string());
    }
    if lexical(path) != lexical(own) {
        return refuse(format!("the lane's tree is {}", own.display()));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return refuse("it is a symbolic link".to_string());
    }
    let real = resolved(path);
    let named = [
        ("the primary checkout", &forbidden.checkouts),
        ("the task worktree", &forbidden.task_worktrees),
        ("another lane's location", &forbidden.other_lanes),
    ];
    for (what, paths) in named {
        for other in paths {
            let other_real = resolved(other);
            if real == other_real {
                return refuse(format!("it is {what} {}", other.display()));
            }
            if real.starts_with(&other_real) {
                return refuse(format!("it is inside {what} {}", other.display()));
            }
            if other_real.starts_with(&real) {
                return refuse(format!("it holds {what} {}", other.display()));
            }
        }
    }
    Ok(real)
}

/// Checks with git that `tree` is a linked worktree of the repository whose primary checkout is
/// `repository`, with its own top at `tree` (not a directory inside another checkout).
async fn check_linked_worktree(git: &Git, tree: &Path, repository: &Path) -> Result<(), String> {
    let top = git
        .run(tree, &["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .await
        .map_err(|error| error.to_string())?;
    if resolved(Path::new(&top)) != resolved(tree) {
        return Err(format!(
            "{} is not a git worktree of its own (git's top is {top}); nothing was reset",
            tree.display()
        ));
    }
    let common_of = async |dir: &Path| {
        git.run(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await
        .map(|text| resolved(Path::new(&text)))
        .map_err(|error| error.to_string())
    };
    let common = common_of(tree).await?;
    if common != common_of(repository).await? {
        return Err(format!(
            "{} is not a worktree of {}; nothing was reset",
            tree.display(),
            repository.display()
        ));
    }
    let own_dir = git
        .run(tree, &["rev-parse", "--path-format=absolute", "--git-dir"])
        .await
        .map_err(|error| error.to_string())?;
    if resolved(Path::new(&own_dir)) == common {
        return Err(format!(
            "{} is a primary checkout, not a lane tree; nothing was reset",
            tree.display()
        ));
    }
    Ok(())
}

/// Adds the lane tree `path` of the repository whose primary checkout is `repository`, detached
/// at `from`, making only the directories below `root` that are missing.
pub async fn add_tree(
    git: &Git,
    repository: &Path,
    path: &Path,
    root: &Path,
    from: &str,
) -> Result<(), String> {
    let parent = path.parent().unwrap_or(path);
    create_below_existing(parent, root).map_err(|error| error.to_string())?;
    let text = path.to_string_lossy();
    git.run(
        repository,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            "--end-of-options",
            &text,
            from,
        ],
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// The tree maker of [`crate::prepare_lane`] for real git: `git worktree add --detach` at
/// `from[repo]` (the task branch or the start of the job), else at the primary checkout's `HEAD`.
pub fn git_tree_maker(git: Git, from: BTreeMap<String, String>) -> MakeTree {
    Arc::new(move |request: &TreeRequest| {
        let from = from.get(&request.repo).map_or("HEAD", String::as_str);
        let path = request.path.to_string_lossy().into_owned();
        // The maker runs on a background thread; waiting for git there is fine.
        futures::executor::block_on(git.run(
            &request.repository_path,
            &["worktree", "add", "-q", "--detach", "--end-of-options", &path, from],
        ))
        .map(|_| ())
        .map_err(anyhow::Error::from)
    })
}

/// One reset of a lane tree at a job start. The tree is the lane's resolved location of `repo`
/// (its second tree when `second`); no other path can be given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetRequest {
    /// The lane's resolved locations, as prepared for this job.
    pub locations: LaneLocations,
    pub repo: String,
    /// The repository's primary checkout.
    pub repository: PathBuf,
    /// Whether this is the lane's second tree; a second tree's work is never saved.
    pub second: bool,
    /// What a missing tree is added at: the task branch, or the start of a project job.
    pub add_from: String,
    /// What the run branch starts from.
    pub start: String,
    pub run_branch: String,
    pub last: Option<LastRun>,
    /// The largest file a save keeps (`lanes.save_max_file_bytes`).
    pub save_max_file_bytes: u64,
    pub forbidden: Forbidden,
}

impl ResetRequest {
    /// The lane's tree of `repo` and the root it may be made below.
    fn location(&self) -> Result<&LaneLocation, String> {
        lane_tree(&self.locations, &self.repo, self.second)
    }

    /// The binding's named directories for this tree.
    fn named_dirs(&self) -> Vec<PathBuf> {
        self.locations
            .items
            .iter()
            .filter(|item| matches!(item.place, Place::Dir { base, .. } if base == self.second))
            .map(|item| item.path.clone())
            .collect()
    }
}

/// The location of `lane`'s tree of `repo` (its second tree when `second`).
pub fn lane_tree<'a>(
    locations: &'a LaneLocations,
    repo: &str,
    second: bool,
) -> Result<&'a LaneLocation, String> {
    locations.get(&tree_name(repo, second)).ok_or_else(|| {
        format!(
            "{} has no {} tree of {repo}; nothing was reset",
            locations.lane,
            if second { "second" } else { "persistent" }
        )
    })
}

/// Where a save went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveTarget {
    /// A commit on the run branch the tree was on.
    Branch(String),
    /// A commit kept at a ref under [`SAVED_REFS`].
    Ref(String),
}

/// What a save before a reset did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The save commit and where it went; `None` when every change was skipped.
    pub commit: Option<(String, SaveTarget)>,
    /// The files too large to keep, with their sizes; the reset removes them.
    pub skipped: Vec<(String, u64)>,
}

impl Saved {
    /// The history entry of the save.
    pub fn history(&self) -> String {
        let mut text = match &self.commit {
            Some((commit, SaveTarget::Branch(branch))) => {
                format!("saved uncommitted work as {} on {branch}", short(commit))
            }
            Some((commit, SaveTarget::Ref(name))) => {
                format!("saved uncommitted work as {} at {name}", short(commit))
            }
            None => "saved no uncommitted work".to_string(),
        };
        if !self.skipped.is_empty() {
            let files: Vec<String> = self
                .skipped
                .iter()
                .map(|(path, size)| format!("{path} ({size} bytes)"))
                .collect();
            text.push_str(&format!("; too large to save: {}", files.join(", ")));
        }
        text
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// What a reset did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetOutcome {
    /// Whether the tree was missing and added now.
    pub added: bool,
    pub saved: Option<Saved>,
    /// The commit the run branch starts at.
    pub head: String,
}

fn git_error(error: GitError) -> String {
    error.to_string()
}

/// Resets the lane tree of `request` onto its new run branch, adding it first when missing. Every
/// check runs before anything is written; a failed save leaves the tree as it was and nothing is
/// reset.
pub async fn reset_tree(git: &Git, request: &ResetRequest) -> Result<ResetOutcome, String> {
    let location = request.location()?;
    let tree = location.path.as_path();
    check_own_tree(tree, &location.path, &request.forbidden)?;
    let added = std::fs::symlink_metadata(tree).is_err();
    if added {
        add_tree(
            git,
            &request.repository,
            tree,
            &location.root,
            &request.add_from,
        )
        .await?;
        // Made now: check it again as it is on disk.
        check_own_tree(tree, &location.path, &request.forbidden)?;
    }
    check_linked_worktree(git, tree, &request.repository).await?;

    let head = git
        .run(
            tree,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", request.start),
            ],
        )
        .await
        .map_err(git_error)?;
    let branch_ref = format!("refs/heads/{}", request.run_branch);
    let existing = git
        .run(tree, &["rev-parse", "-q", "--verify", &branch_ref])
        .await
        .ok();
    if let Some(existing) = existing
        && existing != head
    {
        return Err(format!(
            "the run branch {} already exists at {}; nothing was reset",
            request.run_branch,
            short(&existing)
        ));
    }

    let saved = save(git, request).await?;

    git.run(tree, &["reset", "-q", "--hard"])
        .await
        .map_err(git_error)?;
    let mut clean = vec!["clean".to_string(), "-q".to_string(), "-fd".to_string()];
    for dir in inside(tree, &request.named_dirs()) {
        clean.push(format!("--exclude=/{dir}/"));
    }
    let clean: Vec<&str> = clean.iter().map(String::as_str).collect();
    git.run(tree, &clean).await.map_err(git_error)?;
    git.run(
        tree,
        &[
            "checkout",
            "-q",
            "--force",
            "--no-track",
            "-B",
            &request.run_branch,
            &head,
        ],
    )
    .await
    .map_err(git_error)?;
    Ok(ResetOutcome { added, saved, head })
}

/// The named directories inside `tree`, relative to it, with `/` between components.
fn inside(tree: &Path, dirs: &[PathBuf]) -> Vec<String> {
    let tree = lexical(tree);
    dirs.iter()
        .filter_map(|dir| {
            let relative = lexical(dir).strip_prefix(&tree).ok()?.to_path_buf();
            let text = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

/// The paths `git status --porcelain -z` lists (ignored files excluded), both sides of a rename.
fn changed_paths(status: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut fields = status.split(|byte| *byte == 0).filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        paths.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        // A rename or copy is followed by its source.
        if (code.contains(&b'R') || code.contains(&b'C'))
            && let Some(source) = fields.next()
        {
            paths.push(String::from_utf8_lossy(source).into_owned());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Saves the tree's uncommitted work when the rule says so: only a lane's tip tree, only after a
/// run that failed, halted or was cancelled. Changed files go into one commit made from a
/// separate index (the tree's own index is not touched), on the run branch when the tree is on
/// the last run's branch, else at `refs/bongos/saved/<lane>/<run id>`; never on any other branch.
async fn save(git: &Git, request: &ResetRequest) -> Result<Option<Saved>, String> {
    let Some(last) = &request.last else {
        return Ok(None);
    };
    let Some(word) = last.end.saved_as() else {
        return Ok(None);
    };
    if request.second {
        return Ok(None);
    }
    let tree = request.location()?.path.as_path();
    let refused = |why: String| format!("saving the work of run {} failed: {why}; nothing was reset", last.run_id);
    let status = git
        .output(
            tree,
            &[
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
                "--ignored=no",
                "--no-renames",
            ],
        )
        .await
        .map_err(|error| refused(error.to_string()))?;
    if !status.status.success() {
        return Err(refused(String::from_utf8_lossy(&status.stderr).trim().to_string()));
    }
    let excluded = inside(tree, &request.named_dirs());
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for path in changed_paths(&status.stdout) {
        if excluded
            .iter()
            .any(|dir| path == *dir || path.starts_with(&format!("{dir}/")))
        {
            continue;
        }
        match std::fs::symlink_metadata(tree.join(&path)) {
            Ok(metadata) if metadata.is_file() && metadata.len() > request.save_max_file_bytes => {
                skipped.push((path, metadata.len()));
            }
            Ok(_) => selected.push(path),
            // Deleted: the deletion is saved.
            Err(error) if error.kind() == ErrorKind::NotFound => selected.push(path),
            Err(error) => return Err(refused(format!("reading {path}: {error}"))),
        }
    }
    if selected.is_empty() {
        return Ok((!skipped.is_empty()).then_some(Saved {
            commit: None,
            skipped,
        }));
    }

    let index = git
        .run(
            tree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "bongos-save-index",
            ],
        )
        .await
        .map_err(|error| refused(error.to_string()))?;
    let index = PathBuf::from(index);
    let pathspec = index.with_extension("paths");
    let result = commit_saved(git, request, last, word, &index, &pathspec, &selected).await;
    for file in [&index, &pathspec] {
        std::fs::remove_file(file).ok();
    }
    let commit = result.map_err(refused)?;
    Ok(Some(Saved {
        commit,
        skipped,
    }))
}

async fn commit_saved(
    git: &Git,
    request: &ResetRequest,
    last: &LastRun,
    word: &str,
    index: &Path,
    pathspec: &Path,
    selected: &[String],
) -> Result<Option<(String, SaveTarget)>, String> {
    let tree = request.location()?.path.as_path();
    for file in [index, pathspec] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("removing {}: {error}", file.display())),
        }
    }
    let mut list = Vec::new();
    for path in selected {
        list.extend_from_slice(path.as_bytes());
        list.push(0);
    }
    std::fs::write(pathspec, &list)
        .map_err(|error| format!("writing {}: {error}", pathspec.display()))?;
    let saving = git
        .clone()
        .env("GIT_INDEX_FILE", index)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_AUTHOR_NAME", SAVE_NAME)
        .env("GIT_AUTHOR_EMAIL", SAVE_EMAIL)
        .env("GIT_COMMITTER_NAME", SAVE_NAME)
        .env("GIT_COMMITTER_EMAIL", SAVE_EMAIL);
    let parent = saving
        .run(tree, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(git_error)?;
    saving
        .run(tree, &["read-tree", &parent])
        .await
        .map_err(git_error)?;
    let pathspec_arg = format!("--pathspec-from-file={}", pathspec.display());
    saving
        .run(
            tree,
            &["add", "-A", "--pathspec-file-nul", &pathspec_arg],
        )
        .await
        .map_err(git_error)?;
    let written = saving.run(tree, &["write-tree"]).await.map_err(git_error)?;
    let parent_tree = saving
        .run(tree, &["rev-parse", &format!("{parent}^{{tree}}")])
        .await
        .map_err(git_error)?;
    if written == parent_tree {
        return Ok(None);
    }
    let message = format!("work left by {word} run {}", last.run_id);
    let commit = saving
        .run(tree, &["commit-tree", &written, "-p", &parent, "-m", &message])
        .await
        .map_err(git_error)?;

    let current = git
        .run(tree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    // Only a branch the last run made is ever moved by a save.
    let run_made = last
        .run_branch
        .contains(&format!("-run-{}-", short_id(&last.run_id)));
    if run_made && current == last.run_branch {
        let name = format!("refs/heads/{current}");
        git.run(tree, &["update-ref", &name, &commit, &parent])
            .await
            .map_err(git_error)?;
        return Ok(Some((commit, SaveTarget::Branch(current))));
    }
    let base = format!("{SAVED_REFS}/{}/{}", request.locations.lane, last.run_id);
    let mut number = 1u32;
    loop {
        let name = if number == 1 {
            base.clone()
        } else {
            format!("{base}-{number}")
        };
        let taken = git
            .run(tree, &["rev-parse", "-q", "--verify", &name])
            .await
            .is_ok();
        if !taken {
            // An empty old value: the ref must not exist yet.
            git.run(tree, &["update-ref", &name, &commit, ""])
                .await
                .map_err(git_error)?;
            return Ok(Some((commit, SaveTarget::Ref(name))));
        }
        number += 1;
    }
}

/// What a lane's success merge brings the run branch into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeTarget {
    /// The task branch of a leaf that has no task worktree.
    TaskBranch(String),
    /// A project job's `merge_into`; `None` merges nothing.
    MergeInto(Option<String>),
}

/// One success merge in a lane tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneMerge {
    /// The lane's resolved locations; the merge runs in its persistent tree of `repo`.
    pub locations: LaneLocations,
    pub repo: String,
    pub repository: PathBuf,
    pub forbidden: Forbidden,
    pub target: MergeTarget,
    pub run_branch: String,
    pub run_id: String,
    pub workflow: String,
    pub todo: String,
    pub style: MergeStyle,
}

/// Why a merge in a lane tree did not happen. In every case the run branch is as it was.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaneMergeError {
    #[error("{0}")]
    NotOwnTree(String),
    #[error("{branch} is checked out in {}; nothing was merged", .path.display())]
    CheckedOutElsewhere { branch: String, path: PathBuf },
    #[error("the lane tree has uncommitted changes; nothing was merged")]
    Dirty,
    #[error(transparent)]
    Refused(#[from] MergeRefusal),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// What a merge in a lane tree did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneMerged {
    Merged(Merged),
    /// A project job with no `merge_into`.
    Nothing,
}

/// The worktree (other than `tree`) that has `branch` checked out, if any.
async fn checked_out_elsewhere(
    git: &Git,
    tree: &Path,
    branch: &str,
) -> Result<Option<PathBuf>, GitError> {
    let list = git.run(tree, &["worktree", "list", "--porcelain"]).await?;
    let wanted = format!("branch refs/heads/{branch}");
    let here = resolved(tree);
    let mut current: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if line == wanted
            && let Some(path) = &current
            && resolved(path) != here
        {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}

/// The success merge of a lane run done in the lane's own tree: the target is checked out there
/// (refused while another worktree has it, the user's checkout included), then the run branch is
/// merged in `style` by the shipped merge. With no target nothing happens. The next job's reset
/// moves the tree off the target again.
pub async fn merge_in_lane_tree(
    git: &Git,
    request: &LaneMerge,
) -> Result<LaneMerged, LaneMergeError> {
    let target = match &request.target {
        MergeTarget::TaskBranch(branch) => branch.clone(),
        MergeTarget::MergeInto(None) => return Ok(LaneMerged::Nothing),
        MergeTarget::MergeInto(Some(branch)) => branch.clone(),
    };
    let location =
        lane_tree(&request.locations, &request.repo, false).map_err(LaneMergeError::NotOwnTree)?;
    let tree = location.path.as_path();
    check_own_tree(tree, &location.path, &request.forbidden)
        .map_err(LaneMergeError::NotOwnTree)?;
    check_linked_worktree(git, tree, &request.repository)
        .await
        .map_err(LaneMergeError::NotOwnTree)?;
    if let Some(path) = checked_out_elsewhere(git, tree, &target).await? {
        return Err(LaneMergeError::CheckedOutElsewhere {
            branch: target,
            path,
        });
    }
    if git.is_dirty(tree).await? {
        return Err(LaneMergeError::Dirty);
    }
    git.run(tree, &["checkout", "-q", &target, "--"]).await?;
    let merged = builtin_merge::merge(
        git,
        &MergeRequest {
            task_worktree: location.path.clone(),
            task_branch: target,
            run_branch: request.run_branch.clone(),
            run_id: request.run_id.clone(),
            workflow: request.workflow.clone(),
            todo: request.todo.clone(),
            style: request.style,
        },
    )
    .await?;
    Ok(LaneMerged::Merged(merged))
}
