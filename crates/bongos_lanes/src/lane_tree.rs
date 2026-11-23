//! A lane's persistent tree at a job start, and the success merge done in it.
//!
//! At each start the tree is added when missing (`git worktree add --detach`, below its root
//! only), its uncommitted work is saved when the run that last used it did not succeed, and it
//! is reset: `git reset --hard`, `git clean -fd` (ignored files such as build output stay; never
//! `-x`) and `git checkout --force -B <run branch> <start>`. Before git writes anything, the path
//! is checked to be the lane's own resolved tree, and not a primary checkout, a task worktree or
//! another lane's location, both as written and with links resolved; then git itself must agree
//! that it is a linked worktree of the repository. A job may have written the tree's `.git`, so no
//! git runs in a tree by discovery: the tree's git directory comes from the repository's own
//! record, a crashed job's private git is recovered first ([`crate::lane_git`]), and every command
//! runs through `SafeGit` as a job tree. Nothing here deletes a tree or pushes anything.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_workflow::builtin_merge::{self, MergeRefusal, MergeRequest, MergeStyle, Merged};
use bongos_workflow::worktrees::{Git, GitError, short_id};

use crate::create::{create_below_existing, lexical};
use crate::lane_git::{self, LaneGit};
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
    let refuse = |why: String| {
        Err(format!(
            "{} is not this lane's tree to reset: {why}",
            path.display()
        ))
    };
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

/// Refuses a second tree that is, holds or sits inside one of the lane's own tip trees, with links
/// resolved: its reset must never reach a tree that may hold authored work.
pub(crate) fn check_apart_from_tips(tree: &Path, locations: &LaneLocations) -> Result<(), String> {
    let real = resolved(tree);
    for item in &locations.items {
        if !matches!(item.place, Place::Tree { base: false, .. }) {
            continue;
        }
        let tip = resolved(&item.path);
        if real.starts_with(&tip) || tip.starts_with(&real) {
            return Err(format!(
                "{} is not this lane's second tree to reset: it overlaps its tree {}",
                tree.display(),
                item.path.display()
            ));
        }
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
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                "--end-of-options",
                &path,
                from,
            ],
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

impl Saved {
    /// The history entry of a save made before a halted job's task moves to another lane: "work
    /// left on <branch or ref> at <short sha>", with the files too large to keep.
    pub fn left_on(&self) -> String {
        let mut text = match &self.commit {
            Some((commit, SaveTarget::Branch(name) | SaveTarget::Ref(name))) => {
                format!("work left on {name} at {}", short(commit))
            }
            None => "no work left to save".to_string(),
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

/// Why the work a run left in a lane tree was not saved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SaveError {
    /// The tree cannot be read at all: its directory is missing, or its drive is not mounted.
    #[error("the tree {} cannot be read: {why}", path.display())]
    Unreadable { path: PathBuf, why: String },
    /// The save itself failed (git failed, the disk is full); nothing was written over.
    #[error("{0}")]
    Failed(String),
}

/// Saves the uncommitted work the last run of `request` left in the lane's tree, without
/// resetting it, by the same rule as the save before a reset ([`reset_tree`]): only a tip tree,
/// only after a run that failed, halted or was cancelled. `request.run_branch` names no new run
/// (it may be empty). A save on the run branch moves that branch; the tree's index is then set to
/// the save commit so its files on disk show as committed, and no file is touched. A tree whose
/// directory cannot be read is [`SaveError::Unreadable`]; every other failure leaves the tree and
/// its branches as they were.
pub async fn save_left_work(git: &Git, request: &ResetRequest) -> Result<Option<Saved>, SaveError> {
    if request.second {
        return Ok(None);
    }
    let location = request.location().map_err(SaveError::Failed)?;
    let tree = location.path.as_path();
    let unreadable = |why: String| SaveError::Unreadable {
        path: tree.to_path_buf(),
        why,
    };
    match std::fs::metadata(tree) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(unreadable("it is not a directory".to_string())),
        Err(error) => return Err(unreadable(error.to_string())),
    }
    if let Err(error) = std::fs::read_dir(tree) {
        return Err(unreadable(error.to_string()));
    }
    check_own_tree(tree, &location.path, &request.forbidden).map_err(SaveError::Failed)?;
    let lane = LaneGit::open(git, tree, &request.repository, false)
        .await
        .map_err(SaveError::Failed)?;
    let saved = save(&lane, request).await.map_err(SaveError::Failed)?;
    if let Some(Saved {
        commit: Some((_, SaveTarget::Branch(_))),
        ..
    }) = &saved
    {
        // Only the index: the files on disk stay as the run left them.
        lane.run(&["reset", "-q"])
            .await
            .map_err(|error| SaveError::Failed(git_error(error)))?;
    }
    Ok(saved)
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
/// reset. A second tree is only reset and cleaned where it stands, never saved or moved to a branch.
pub async fn reset_tree(git: &Git, request: &ResetRequest) -> Result<ResetOutcome, String> {
    let location = request.location()?;
    let tree = location.path.as_path();
    check_own_tree(tree, &location.path, &request.forbidden)?;
    if request.second {
        check_apart_from_tips(tree, &request.locations)?;
    }
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
    let lane = LaneGit::open(git, tree, &request.repository, added).await?;

    if request.second {
        return reset_second(&lane, request, added).await;
    }

    let head = lane
        .run(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{}^{{commit}}", request.start),
        ])
        .await
        .map_err(git_error)?;
    let branch_ref = format!("refs/heads/{}", request.run_branch);
    if let Some(existing) = branch_at(&lane, &branch_ref).await
        && existing != head
    {
        return Err(format!(
            "the run branch {} already exists at {}; nothing was reset",
            request.run_branch,
            short(&existing)
        ));
    }

    let saved = save(&lane, request).await?;

    // Checked again after the save: `checkout -B` below must never move a branch off a commit
    // the save (or anything else) put on it.
    if let Some(existing) = branch_at(&lane, &branch_ref).await
        && existing != head
    {
        return Err(format!(
            "the run branch {} is at {} after the save, not at its start {}; nothing was reset",
            request.run_branch,
            short(&existing),
            short(&head)
        ));
    }

    // The job tree's commands never recurse into submodules, whatever the user's configuration.
    lane.run(&["reset", "-q", "--hard"])
        .await
        .map_err(git_error)?;
    clean_tree(&lane, request).await?;
    lane.run(&[
        "checkout",
        "-q",
        "--force",
        "--no-track",
        "-B",
        &request.run_branch,
        &head,
    ])
    .await
    .map_err(git_error)?;
    Ok(ResetOutcome { added, saved, head })
}

/// `git clean -fd` in `tree`, keeping the binding's named directories of this tree that sit inside
/// it (and, as always, ignored files).
async fn clean_tree(lane: &LaneGit, request: &ResetRequest) -> Result<(), String> {
    let mut clean = vec!["clean".to_string(), "-q".to_string(), "-fd".to_string()];
    for dir in inside(lane.tree(), &request.named_dirs()) {
        clean.push(format!("--exclude=/{dir}/"));
    }
    let clean: Vec<&str> = clean.iter().map(String::as_str).collect();
    lane.run(&clean).await.map_err(git_error)?;
    Ok(())
}

/// The reset of a second tree, already checked to be the lane's own linked worktree: `git reset
/// --hard` and `git clean -fd` where it stands. It holds no authored work, so nothing is saved,
/// and it never takes a branch (the tip tree has the run branch), so `HEAD` stays where it is.
async fn reset_second(
    lane: &LaneGit,
    request: &ResetRequest,
    added: bool,
) -> Result<ResetOutcome, String> {
    lane.run(&["reset", "-q", "--hard"])
        .await
        .map_err(git_error)?;
    clean_tree(lane, request).await?;
    let head = lane
        .run(&["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(git_error)?;
    Ok(ResetOutcome {
        added,
        saved: None,
        head,
    })
}

/// The commit `branch_ref` is at, `None` when it does not exist.
async fn branch_at(lane: &LaneGit, branch_ref: &str) -> Option<String> {
    lane.run(&["rev-parse", "-q", "--verify", branch_ref])
        .await
        .ok()
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

/// A path git printed, byte for byte (a file name need not be UTF-8).
#[cfg(unix)]
fn path_from_git(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_git(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// A path's bytes as git reads them from a pathspec file.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

/// The paths `git status --porcelain -z` lists (ignored files excluded), both sides of a rename.
fn changed_paths(status: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut fields = status
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        paths.push(path_from_git(&entry[3..]));
        // A rename or copy is followed by its source.
        if (code.contains(&b'R') || code.contains(&b'C'))
            && let Some(source) = fields.next()
        {
            paths.push(path_from_git(source));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Writes `paths` NUL-separated, as `--pathspec-file-nul` reads them.
fn write_pathspec(file: &Path, paths: &[&Path]) -> Result<(), String> {
    let mut list = Vec::new();
    for path in paths {
        list.extend_from_slice(&path_bytes(path));
        list.push(0);
    }
    std::fs::write(file, &list).map_err(|error| format!("writing {}: {error}", file.display()))
}

/// The changes a save keeps, sorted out from the status.
struct Changes {
    /// Changed paths present on disk: added from the disk.
    present: Vec<PathBuf>,
    /// Changed paths gone from the disk: removed.
    gone: Vec<PathBuf>,
    /// Changed paths left out (too large, or in a named directory): kept as at `HEAD`.
    dropped: Vec<PathBuf>,
}

/// Saves the tree's uncommitted work when the rule says so: only a lane's tip tree, only after a
/// run that failed, halted or was cancelled. Changes staged in the tree's index go into one
/// commit, then the changed files on disk into another on top of it, each made only when it
/// changes something and each from a separate index (the tree's own index is not touched). The
/// save goes on the run branch when the tree is on the last run's branch and the new run does not
/// reuse that branch, else at `refs/bongos/saved/<lane>/<run id>`; never on any other branch.
/// No hook runs and nothing is signed.
///
/// A last run that is not known (none given, the tree is on another run's run branch, as after
/// that run's record was cleared, or the last run succeeded and the tree is dirty off its own
/// branch) never lets work go: the tree is saved as left by an unrecorded
/// run, at a saved ref, whatever the given run's end. Commits only a detached `HEAD` holds are kept
/// at a saved ref too.
async fn save(lane: &LaneGit, request: &ResetRequest) -> Result<Option<Saved>, String> {
    if request.second {
        return Ok(None);
    }
    let current = lane
        .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    let unrecorded = LastRun {
        run_id: UNRECORDED.to_string(),
        run_branch: String::new(),
        end: LastEnd::Halted,
    };
    // A succeeded run's work is merged, so nothing of it is saved, but a tree left dirty off
    // that run's own branch holds work no run recorded.
    let stray = match &request.last {
        Some(last) if last.end == LastEnd::Succeeded && current != last.run_branch => {
            lane.is_dirty().await.map_err(git_error)?
        }
        _ => false,
    };
    let last = match &request.last {
        Some(last) if !stray && !on_other_run_branch(&current, &last.run_id) => last,
        _ => &unrecorded,
    };
    let saved = save_changes(lane, request, last).await?;
    if !current.is_empty()
        || matches!(
            &saved,
            Some(Saved {
                commit: Some(_),
                ..
            })
        )
    {
        return Ok(saved);
    }
    // Detached: a commit made there is on no branch, and the reset would leave it behind.
    let head = lane
        .run(&["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(git_error)?;
    let holders = lane
        .run(&[
            "for-each-ref",
            "--count=1",
            "--contains",
            &head,
            "--format=%(refname)",
        ])
        .await
        .map_err(git_error)?;
    if !holders.is_empty() {
        return Ok(saved);
    }
    let name = saved_ref(lane, request, last, &head).await?;
    Ok(Some(Saved {
        commit: Some((head, SaveTarget::Ref(name))),
        skipped: saved.map(|saved| saved.skipped).unwrap_or_default(),
    }))
}

/// The run id of a save whose last run is not known.
pub const UNRECORDED: &str = "unrecorded";

/// Whether `branch` is the run branch (`<task>-run-<short>-<attempt>`) of a run other than `run_id`.
fn on_other_run_branch(branch: &str, run_id: &str) -> bool {
    let Some((rest, attempt)) = branch.rsplit_once('-') else {
        return false;
    };
    let Some((_, short)) = rest.rsplit_once("-run-") else {
        return false;
    };
    let is_run_branch = !attempt.is_empty()
        && attempt.chars().all(|c| c.is_ascii_digit())
        && short.len() == 8
        && short.chars().all(|c| c.is_ascii_hexdigit());
    is_run_branch && short != short_id(run_id)
}

/// Keeps `commit` at a new ref `refs/bongos/saved/<lane>/<run id>` (`-2`, `-3`, ... when taken).
async fn saved_ref(
    lane: &LaneGit,
    request: &ResetRequest,
    last: &LastRun,
    commit: &str,
) -> Result<String, String> {
    let base = format!("{SAVED_REFS}/{}/{}", request.locations.lane, last.run_id);
    let mut number = 1u32;
    loop {
        let name = if number == 1 {
            base.clone()
        } else {
            format!("{base}-{number}")
        };
        let taken = lane
            .run(&["rev-parse", "-q", "--verify", &name])
            .await
            .is_ok();
        if !taken {
            // An empty old value: the ref must not exist yet.
            lane.run(&["update-ref", &name, commit, ""])
                .await
                .map_err(git_error)?;
            return Ok(name);
        }
        number += 1;
    }
}

/// The save of the changes in `tree` left by `last`, when its end says to save them.
async fn save_changes(
    lane: &LaneGit,
    request: &ResetRequest,
    last: &LastRun,
) -> Result<Option<Saved>, String> {
    let tree = lane.tree();
    let Some(word) = last.end.saved_as() else {
        return Ok(None);
    };
    let word = if last.run_id == UNRECORDED {
        "an unrecorded run".to_string()
    } else {
        format!("{word} run {}", last.run_id)
    };
    let word = word.as_str();
    let refused = |why: String| {
        format!(
            "saving the work of run {} failed: {why}; nothing was reset",
            last.run_id
        )
    };
    // `status` runs with no optional locks: it must not rewrite the tree's index.
    let status = lane
        .output(&[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--no-renames",
        ])
        .await
        .map_err(|error| refused(error.to_string()))?;
    if !status.status.success() {
        return Err(refused(
            String::from_utf8_lossy(&status.stderr).trim().to_string(),
        ));
    }
    let excluded = inside(tree, &request.named_dirs());
    let mut changes = Changes {
        present: Vec::new(),
        gone: Vec::new(),
        dropped: Vec::new(),
    };
    let mut skipped = Vec::new();
    for path in changed_paths(&status.stdout) {
        if excluded.iter().any(|dir| path.starts_with(dir)) {
            changes.dropped.push(path);
            continue;
        }
        match std::fs::symlink_metadata(tree.join(&path)) {
            Ok(metadata) if metadata.is_file() && metadata.len() > request.save_max_file_bytes => {
                skipped.push((path.to_string_lossy().into_owned(), metadata.len()));
                changes.dropped.push(path);
            }
            Ok(_) => changes.present.push(path),
            // Deleted (or a parent is now a file): the deletion is saved.
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                changes.gone.push(path)
            }
            Err(error) => {
                return Err(refused(format!("reading {}: {error}", path.display())));
            }
        }
    }
    if changes.present.is_empty() && changes.gone.is_empty() {
        return Ok((!skipped.is_empty()).then_some(Saved {
            commit: None,
            skipped,
        }));
    }

    // The tree's own git directory (per worktree, not the shared one).
    let files = SaveFiles::at(lane.git_dir().join("bongos-save-index"));
    let result = commit_saved(lane, request, last, word, &files, &changes).await;
    for file in files.all() {
        std::fs::remove_file(file).ok();
    }
    let commit = result.map_err(refused)?;
    Ok(Some(Saved { commit, skipped }))
}

/// The scratch files of a save, all in the tree's git directory.
struct SaveFiles {
    index: PathBuf,
    present: PathBuf,
    gone: PathBuf,
    dropped: PathBuf,
}

impl SaveFiles {
    fn at(index: PathBuf) -> Self {
        Self {
            present: index.with_extension("paths"),
            gone: index.with_extension("gone"),
            dropped: index.with_extension("dropped"),
            index,
        }
    }

    fn all(&self) -> [&Path; 4] {
        [&self.index, &self.present, &self.gone, &self.dropped]
    }
}

async fn commit_saved(
    lane: &LaneGit,
    request: &ResetRequest,
    last: &LastRun,
    word: &str,
    files: &SaveFiles,
    changes: &Changes,
) -> Result<Option<(String, SaveTarget)>, String> {
    for file in files.all() {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("removing {}: {error}", file.display())),
        }
    }
    let own_index = lane.git_dir().join("index");
    let saving = lane.with_git(
        lane.git()
            .clone()
            .env("GIT_INDEX_FILE", &files.index)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_AUTHOR_NAME", SAVE_NAME)
            .env("GIT_AUTHOR_EMAIL", SAVE_EMAIL)
            .env("GIT_COMMITTER_NAME", SAVE_NAME)
            .env("GIT_COMMITTER_EMAIL", SAVE_EMAIL),
    );
    let run = async |args: &[&str]| saving.run(args).await.map_err(git_error);
    let commit = async |written: &str, parent: &str, message: &str| {
        run(&[
            "commit-tree",
            "--no-gpg-sign",
            written,
            "-p",
            parent,
            "-m",
            message,
        ])
        .await
    };
    let parent = run(&["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    let parent_tree = run(&["rev-parse", &format!("{parent}^{{tree}}")]).await?;
    let mut top = parent.clone();
    let mut top_tree = parent_tree.clone();

    // What is staged, from a copy of the tree's index with the left-out paths as at HEAD. An
    // index git cannot write as a tree (a conflict) is not saved; its files on disk are.
    let staged = match std::fs::copy(&own_index, &files.index) {
        Ok(_) => {
            if !changes.dropped.is_empty() {
                let dropped: Vec<&Path> = changes.dropped.iter().map(PathBuf::as_path).collect();
                write_pathspec(&files.dropped, &dropped)?;
                let arg = format!("--pathspec-from-file={}", files.dropped.display());
                run(&["reset", "-q", &parent, "--pathspec-file-nul", &arg]).await?;
            }
            run(&["write-tree"]).await.ok()
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(format!("copying {}: {error}", own_index.display())),
    };
    if let Some(staged) = staged
        && staged != top_tree
    {
        let message = format!("staged work left by {word}");
        top = commit(&staged, &top, &message).await?;
        top_tree = staged;
    }

    // The files on disk, from HEAD: present ones added, gone ones removed. A path git does not
    // know is no error either way.
    run(&["read-tree", &parent]).await?;
    let present: Vec<&Path> = changes.present.iter().map(PathBuf::as_path).collect();
    if !present.is_empty() {
        write_pathspec(&files.present, &present)?;
        let arg = format!("--pathspec-from-file={}", files.present.display());
        // `--force`: a listed path that git ignores was added to the index by hand; keep it.
        run(&["add", "-A", "--force", "--pathspec-file-nul", &arg]).await?;
    }
    let gone: Vec<&Path> = changes.gone.iter().map(PathBuf::as_path).collect();
    if !gone.is_empty() {
        write_pathspec(&files.gone, &gone)?;
        let arg = format!("--pathspec-from-file={}", files.gone.display());
        run(&[
            "rm",
            "-q",
            "-r",
            "--force",
            "--cached",
            "--ignore-unmatch",
            "--pathspec-file-nul",
            &arg,
        ])
        .await?;
    }
    let written = run(&["write-tree"]).await?;
    if written != top_tree {
        let message = format!("work left by {word}");
        top = commit(&written, &top, &message).await?;
    }
    if top == parent {
        return Ok(None);
    }
    let commit = top;

    let current = lane
        .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    // Only a branch the last run made is ever moved by a save.
    let run_made = last
        .run_branch
        .contains(&format!("-run-{}-", short_id(&last.run_id)));
    // A new run that reuses the branch (the same run started again) puts it back at its start:
    // the save then goes to a saved ref, so moving the branch cannot drop it.
    if run_made && current == last.run_branch && current != request.run_branch {
        let name = format!("refs/heads/{current}");
        lane.run(&["update-ref", &name, &commit, &parent])
            .await
            .map_err(git_error)?;
        return Ok(Some((commit, SaveTarget::Branch(current))));
    }
    let name = saved_ref(lane, request, last, &commit).await?;
    Ok(Some((commit, SaveTarget::Ref(name))))
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

/// The worktree (other than `tree`) that has `branch` checked out, if any. Asked of the user's
/// own checkout `repository`: the list is the same from every worktree, and no git runs in the
/// lane tree by discovery.
async fn checked_out_elsewhere(
    git: &Git,
    repository: &Path,
    tree: &Path,
    branch: &str,
) -> Result<Option<PathBuf>, GitError> {
    let list = git
        .run(repository, &["worktree", "list", "--porcelain"])
        .await?;
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
    check_own_tree(tree, &location.path, &request.forbidden).map_err(LaneMergeError::NotOwnTree)?;
    let lane = LaneGit::open(git, tree, &request.repository, false)
        .await
        .map_err(LaneMergeError::NotOwnTree)?;
    if let Some(path) = checked_out_elsewhere(git, &request.repository, tree, &target).await? {
        return Err(LaneMergeError::CheckedOutElsewhere {
            branch: target,
            path,
        });
    }
    if lane.is_dirty().await? {
        return Err(LaneMergeError::Dirty);
    }
    lane.run(&["checkout", "-q", &target, "--"]).await?;
    // The shipped merge names the lane tree's git directory, which it trusts only as a linked
    // worktree of the repository's common directory, read in the person's own checkout.
    let common = lane_git::common_dir(git, &request.repository).await?;
    let merged = builtin_merge::merge(
        git,
        &MergeRequest {
            task_worktree: location.path.clone(),
            run_git_dir: common,
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
