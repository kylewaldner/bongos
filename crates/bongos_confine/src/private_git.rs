//! Private git for a job in a linked worktree: the job's commits and branches land in a git
//! directory of its own in the job's scratch, never in the shared repository.
//!
//! [`prepare`] turns the tree's `.git` file (`gitdir: <linked git dir>`) into a pointer at
//! `<scratch>/git`, a standalone repository that starts as a snapshot of the shared one: `HEAD`
//! names the branch the linked worktree has checked out, `packed-refs` holds the shared
//! `refs/heads`, `refs/tags` and `refs/remotes` (so `git diff origin/main` works), the index is a
//! copy of the worktree's, and the objects are borrowed through `objects/info/alternates` from
//! the shared object directory, so nothing is copied and no object is written. It is not a linked
//! worktree of the shared repository (no `commondir`), so neither the shared configuration nor
//! its hooks are read by the job's git; only the repository format (`core.repositoryformatversion`,
//! `extensions.objectFormat`) and the booleans `git init` probes from the file system
//! (`core.filemode`, `core.symlinks`, `core.ignorecase`, `core.precomposeunicode`) are carried
//! over. The shared `info/exclude` and `shallow` files are copied too, since git in the tree would
//! otherwise list excluded files as untracked or walk past a shallow boundary into missing
//! commits. [`PrivateGit::import`] brings the job's commits and branches into the shared
//! repository and switches the tree back; [`PrivateGit::abandon`] switches it back without
//! importing anything.
//!
//! # Before the job starts
//!
//! The scratch directory belongs to the host until the sandbox starts, so [`prepare`] must run
//! before any process of the job exists in the scratch (and in the tree). Its file creations
//! refuse to follow a symlink only in the last path component; a job that could already rename
//! or replace a directory on the way to the private directory could redirect them.
//!
//! # The marker
//!
//! A tree is taken by the marker `<linked git dir>/bongos-confined`, which names the owner (pid
//! and process start time), the tree, the private directory, the original `.git` text and, once
//! taken, the snapshot of refs. The marker is created whole: written to a temporary file, then
//! hard-linked to its name, which fails when a marker is already there. So a marker is never
//! seen half written, and two preparations of one tree never both take it. Every step that reads
//! or changes the marker is serialized by an advisory lock on `<linked git dir>/
//! bongos-confined.lock` (only tried, never waited for: a held lock means another job is
//! preparing the tree). Different trees of one repository have different linked git directories,
//! so they are independent.
//!
//! An owner counts as alive while a process with its pid runs and (on Linux) started at the
//! recorded start time; elsewhere a running process with the pid counts as the owner, so a reused
//! pid keeps the tree taken rather than freeing it. A marker whose owner is gone is recovered
//! ([`recover`], also run by [`prepare`] before taking the tree): the tree's `.git` is restored to
//! the original text, the private directory is removed without following symlinks, then the
//! marker is removed, in that order, so a recovery cut short is finished by the next one. A
//! marker that cannot be parsed is never guessed at: it can only come from outside bongos (a
//! crash cannot leave one half written), so preparing and recovering refuse with an error naming
//! it, and nothing is changed until someone removes it by hand.
//!
//! # Running git against the private directory
//!
//! The private directory lies in the job's scratch, a registered job root, so
//! [`SafeGit`] refuses it as a git directory: its configuration, hooks, `HEAD` and refs are
//! job-written. Every git command this module runs is on the shared repository (through the
//! linked git directory, which is not job-writable), on the tree with the linked git directory,
//! or on the import directory below. None ever uses the private directory as a git directory, so
//! no private configuration, hook or fsmonitor ever runs.
//!
//! # Importing
//!
//! [`PrivateGit::import`] runs once every process of the job is gone. It holds the marker's lock
//! throughout, and refuses unless the marker still names this preparation.
//!
//! 1. The private refs are read as data, never by git: `HEAD`, the loose files under `refs/`
//!    (walked without following symlinks; each file opened without following a symlink or
//!    blocking, so a symlink, FIFO or other special file is a refused entry) and `packed-refs`,
//!    each bounded in size, depth and count. A loose ref overrides a packed one, and a name
//!    refused in either never falls back to the other. Each name must pass the rules of
//!    `git check-ref-format` (checked in Rust) and each value must be a full object id of the
//!    repository's object format (taken from the snapshot); a symbolic ref is refused.
//! 2. The refs are compared with the snapshot. The own branch may move anywhere (a rewrite too)
//!    and a new branch under `refs/heads/` may be created; everything else that changed is
//!    refused and reported, never imported: another branch moved or deleted, the own branch
//!    deleted, any tag, note, replace ref, remote ref or other namespace. `HEAD` never travels:
//!    when it names another branch, that branch follows the rules above and the report notes
//!    that the tree is back on the own branch; when it is detached at a commit no imported or
//!    snapshot ref reaches (asked of the shared repository after the import), that is a refused
//!    entry.
//! 3. With refs to import, the private `objects/` must hold only loose objects, plain packs
//!    (`pack/pack-*.pack` and `.idx`) and the `info/alternates` prepare wrote; a symlink, a
//!    changed alternates, a commit graph, multi-pack index, bitmap, reverse index or anything
//!    else fails the import. Then the import directory `<linked git dir>/bongos-import` is made:
//!    the linked git directory belongs to the host and lies outside every job root, so the job
//!    cannot write it and [`SafeGit`] accepts it as a git directory. It is a bare repository
//!    made by `git init --template=`, whose `objects/info/alternates` names the private then the
//!    shared object directory; reading the private one is the one exception
//!    [`SafeGit::job_object_directory`] grants, with commit graphs, multi-pack indexes, bitmaps
//!    and reverse indexes switched off. There each new tip must be a commit (`cat-file`; a ref
//!    naming anything else is refused), then `pack-objects --revs --thin` of the new tips
//!    `--not` the snapshot tips streams into `index-pack --stdin --fix-thin --strict` in the
//!    shared repository, so only new objects travel and each is checked. Then `rev-list
//!    --objects --missing=error` of the same range in the shared repository (the same settings
//!    switched off) checks that every object arrived before any ref moves.
//! 4. One `update-ref --stdin` transaction writes the refs: the own branch only from its snapshot
//!    value, a new branch only where none exists. A ref that moved in the shared repository
//!    fails the whole transaction ("branch <b> moved while the job ran; nothing imported"); a
//!    stale lock fails it with git's own message, and bongos never removes a git lock.
//! 5. The tree is switched back: `.git` is restored by an atomic rename, the shared index is
//!    reset to the own branch when the import moved it (`reset -q` in the tree, keeping the
//!    working files, so changes the job staged but did not commit become unstaged; when host git
//!    refuses the tree, a nested repository in it for one, `read-tree` without the work tree
//!    instead), the import directory and the private directory are removed and the marker last.
//!
//! A pass with no ref to import packs nothing and makes no import directory; with the own branch
//! unmoved the shared index still matches it, so only the switch back runs. When anything was
//! refused the allowed refs are imported all the same, and the private directory is kept (named
//! in the report) so no commit is lost; the next [`prepare`] into the same scratch replaces it.
//! When the import fails (a host git for another git directory, the object check, packing, the
//! connectivity check, the private alternates changed after packing, or the transaction) no ref
//! moves, the tree is switched back and the private directory kept the same way, provided the
//! marker still names this preparation. Failing to remove the private directory or the marker
//! after refs moved is logged and noted in the report, never returned in its place. Objects
//! that reached the shared repository before a failure or a crash are unreferenced and harmless.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::safe_git::{SafeGit, linked_git_dir_from_record};

#[path = "private_git_import.rs"]
mod import;

pub use import::ImportReport;
#[cfg(test)]
pub(crate) use import::check_ref_format;

/// The marker's name in the linked git directory.
pub const MARKER: &str = "bongos-confined";

/// The lock serializing every change to the marker, in the linked git directory.
const LOCK: &str = "bongos-confined.lock";

/// The private git directory's name in the scratch directory.
pub const PRIVATE_DIR: &str = "git";

/// The refusal when the tree is taken.
pub const BUSY: &str = "another job is using this tree";

/// The refusal when the tree's branch has no commit.
pub const UNBORN: &str = "the branch has no commit yet";

/// The refusal when the tree's `HEAD` is detached.
pub const DETACHED: &str = "the tree's HEAD is detached; private git needs a checked-out branch";

/// The first line of a marker.
const MARKER_HEADER: &str = "bongos-confined 1";

/// The largest marker read.
/// The largest `.git` file read back.
const DOT_GIT_LIMIT: u64 = 64 << 10;
const MARKER_LIMIT: u64 = 256 << 20;

/// The ref namespaces the snapshot copies.
const SNAPSHOT_PREFIXES: &[&str] = &["refs/heads/", "refs/tags/", "refs/remotes/"];

/// The file system booleans copied from the shared configuration.
const FILE_SYSTEM_BOOLEANS: &[&str] = &[
    "core.filemode",
    "core.symlinks",
    "core.ignorecase",
    "core.precomposeunicode",
];

/// A tree switched to private git. Holds what importing the job's work needs: the tree, its
/// linked git directory, the private directory, the branch and the snapshot of shared refs taken
/// when the tree was prepared, and the original `.git` text.
#[derive(Debug)]
pub struct PrivateGit {
    tree: PathBuf,
    linked_git_dir: PathBuf,
    common_dir: PathBuf,
    private_dir: PathBuf,
    branch: String,
    snapshot: BTreeMap<String, String>,
    original_dot_git: Vec<u8>,
    pid: u32,
    start_time: u64,
}

impl PrivateGit {
    /// The tree, canonical.
    pub fn tree(&self) -> &Path {
        &self.tree
    }

    /// The tree's linked git directory in the shared repository, canonical.
    pub fn linked_git_dir(&self) -> &Path {
        &self.linked_git_dir
    }

    /// The shared repository's common directory, canonical.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The private git directory, `<scratch>/git`.
    pub fn private_dir(&self) -> &Path {
        &self.private_dir
    }

    /// The branch the tree has checked out (`refs/heads/<name>`).
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The shared `refs/heads`, `refs/tags` and `refs/remotes` when the tree was prepared: each
    /// full ref name with its object id.
    pub fn snapshot(&self) -> &BTreeMap<String, String> {
        &self.snapshot
    }

    /// The tree's `.git` file as it was before [`prepare`].
    pub fn original_dot_git(&self) -> &[u8] {
        &self.original_dot_git
    }

    /// Switches the tree back to the shared repository without importing anything: restores the
    /// tree's `.git`, removes the private directory (without following symlinks), then the
    /// marker. Refuses, changing nothing, when the marker no longer names this preparation. When
    /// a step fails the marker stays, so [`recover`] finishes the job once this process is gone.
    pub fn abandon(self) -> Result<()> {
        // No lock is needed: only the owner, alive here, changes a marker whose owner is alive,
        // and the marker goes last, so whoever sees it gone sees the tree restored.
        self.check_marker()?;
        undo(
            &self.linked_git_dir,
            &self.tree,
            &self.private_dir,
            &self.original_dot_git,
        )
        .with_context(|| format!("switching {} back to shared git", self.tree.display()))
    }

    /// Refuses unless the marker names this preparation.
    fn check_marker(&self) -> Result<()> {
        let marker = Marker::read(&self.linked_git_dir)?;
        match &marker {
            Some(marker)
                if marker.pid == self.pid
                    && marker.start_time == self.start_time
                    && marker.private_dir == self.private_dir
                    && marker.tree == self.tree => {}
            Some(_) => bail!(
                "the marker in {} names another job; leaving the tree {} as it is",
                self.linked_git_dir.display(),
                self.tree.display()
            ),
            None => bail!(
                "the marker in {} is gone; leaving the tree {} as it is",
                self.linked_git_dir.display(),
                self.tree.display()
            ),
        }
        Ok(())
    }
}

/// What [`recover`] found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverOutcome {
    /// No marker: nothing to recover.
    NoMarker,
    /// The owner was gone; the tree is restored and the marker removed.
    Recovered,
    /// The marker's owner is alive, or another job is preparing the tree right now; nothing was
    /// changed.
    InUse,
}

/// Switches the linked worktree whose git directory is `linked_git_dir` to private git (see the
/// module documentation). Recovers a marker whose owner is gone first; refuses with [`BUSY`]
/// while another job holds the tree. Any failure after the tree is taken undoes every step done:
/// the private directory is removed, the `.git` is left (or put back) byte for byte, and the
/// marker is removed. Call it before any process of the job exists in the scratch (see the module
/// documentation).
pub async fn prepare(tree: &Path, linked_git_dir: &Path, scratch: &Path) -> Result<PrivateGit> {
    prepare_with(tree, linked_git_dir, scratch, &|git| git).await
}

/// [`prepare`], with `configure` adjusting the git command run on the shared repository (the git
/// binary, the environment).
pub(crate) async fn prepare_with(
    tree: &Path,
    linked_git_dir: &Path,
    scratch: &Path,
    configure: &(dyn Fn(SafeGit) -> SafeGit + Sync),
) -> Result<PrivateGit> {
    let tree = canonical(tree, "tree")?;
    let linked_git_dir = canonical(linked_git_dir, "linked git directory")?;
    let scratch = canonical(scratch, "scratch directory")?;
    let _lock = TreeLock::try_take(&linked_git_dir)?
        .ok_or_else(|| anyhow!("{BUSY}: {}", tree.display()))?;

    if let Some(marker) = Marker::read(&linked_git_dir)? {
        if owner_alive(marker.pid, marker.start_time) {
            bail!("{BUSY}: {} (process {})", tree.display(), marker.pid);
        }
        log::info!(
            "private git: recovering {} from a job that is gone (process {})",
            marker.tree.display(),
            marker.pid
        );
        marker
            .undo(&linked_git_dir)
            .with_context(|| format!("recovering {} before preparing it", tree.display()))?;
    }

    let common_dir = common_dir(&linked_git_dir)?;
    let recorded = linked_git_dir_from_record(&tree, &common_dir)?;
    if recorded != linked_git_dir {
        bail!(
            "the .git of {} names {}, not {}",
            tree.display(),
            recorded.display(),
            linked_git_dir.display()
        );
    }
    let dot_git = tree.join(".git");
    let original_dot_git = read_regular_no_follow(&dot_git)?;
    let private_dir = scratch.join(PRIVATE_DIR);
    let (pid, start_time) = this_owner()?;
    let mut marker = Marker {
        pid,
        start_time,
        tree: tree.clone(),
        private_dir: private_dir.clone(),
        original_dot_git: original_dot_git.clone(),
        branch: None,
        snapshot: None,
    };
    marker.create(&linked_git_dir)?;

    let git = configure(SafeGit::without_work_tree(&linked_git_dir));
    match switch(&git, &common_dir, &linked_git_dir, &mut marker).await {
        Ok((branch, snapshot)) => Ok(PrivateGit {
            tree,
            linked_git_dir,
            common_dir,
            private_dir,
            branch,
            snapshot,
            original_dot_git,
            pid,
            start_time,
        }),
        Err(error) => {
            if let Err(undo_error) = marker.undo(&linked_git_dir) {
                log::error!(
                    "private git: cannot undo preparing {}: {undo_error:#}",
                    tree.display()
                );
                return Err(error.context(format!(
                    "preparing private git failed, and undoing it failed too ({undo_error:#}); \
                     the marker in {} is left for recovery",
                    linked_git_dir.display()
                )));
            }
            Err(error)
        }
    }
}

/// Recovers the tree of `linked_git_dir` from a job that is gone (see the module documentation).
/// Idempotent: with no marker it does nothing.
pub fn recover(linked_git_dir: &Path) -> Result<RecoverOutcome> {
    let linked_git_dir = canonical(linked_git_dir, "linked git directory")?;
    let Some(_lock) = TreeLock::try_take(&linked_git_dir)? else {
        return Ok(RecoverOutcome::InUse);
    };
    let Some(marker) = Marker::read(&linked_git_dir)? else {
        return Ok(RecoverOutcome::NoMarker);
    };
    if owner_alive(marker.pid, marker.start_time) {
        return Ok(RecoverOutcome::InUse);
    }
    marker.undo(&linked_git_dir)?;
    Ok(RecoverOutcome::Recovered)
}

/// The steps of [`prepare`] after the marker: reads the shared repository, writes the private
/// directory, records the snapshot in the marker, then redirects the tree's `.git`.
async fn switch(
    git: &SafeGit,
    common_dir: &Path,
    linked_git_dir: &Path,
    marker: &mut Marker,
) -> Result<(String, BTreeMap<String, String>)> {
    let branch = head_branch(git).await?;
    let snapshot = snapshot_refs(git).await?;
    if !snapshot.contains_key(&branch) {
        bail!("{UNBORN}: {branch}");
    }
    let config = shared_config(git).await?;

    let private_dir = marker.private_dir.clone();
    write_private_dir(
        &private_dir,
        linked_git_dir,
        common_dir,
        &branch,
        &snapshot,
        &config,
    )?;

    marker.branch = Some(branch.clone());
    marker.snapshot = Some(snapshot.clone());
    marker.replace(linked_git_dir)?;

    let mut redirect = b"gitdir: ".to_vec();
    redirect.extend_from_slice(line_bytes(&private_dir, "private git directory")?);
    redirect.push(b'\n');
    replace_file(&marker.tree.join(".git"), &redirect)?;
    Ok((branch, snapshot))
}

/// Runs `args` on the shared repository; an exit status other than 0 or one in `allowed` fails.
async fn run_git(git: &SafeGit, args: &[&str], allowed: &[i32]) -> Result<std::process::Output> {
    let describe = || format!("git {}", args.join(" "));
    let output = git
        .command(args)
        .await
        .with_context(describe)?
        .output()
        .await
        .with_context(describe)?;
    let code = output.status.code();
    if !output.status.success() && !code.is_some_and(|code| allowed.contains(&code)) {
        bail!(
            "{} failed: {}",
            describe(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// The branch the linked worktree's `HEAD` names, as `refs/heads/<name>`.
async fn head_branch(git: &SafeGit) -> Result<String> {
    let output = run_git(git, &["symbolic-ref", "-q", "HEAD"], &[1]).await?;
    if !output.status.success() {
        bail!("{DETACHED}");
    }
    let text = String::from_utf8(output.stdout).context("HEAD names a ref that is not UTF-8")?;
    let branch = text.strip_suffix('\n').unwrap_or(&text);
    if !branch.starts_with("refs/heads/") || !valid_ref_name(branch) {
        bail!("the tree's HEAD names {branch:?}, not a branch");
    }
    Ok(branch.to_string())
}

/// The shared `refs/heads`, `refs/tags` and `refs/remotes`, each with its object id. A symbolic
/// ref (`refs/remotes/origin/HEAD`) is recorded with the object id it resolves to.
async fn snapshot_refs(git: &SafeGit) -> Result<BTreeMap<String, String>> {
    let output = run_git(
        git,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads/",
            "refs/tags/",
            "refs/remotes/",
        ],
        &[],
    )
    .await?;
    let text = String::from_utf8(output.stdout).context("a shared ref name is not UTF-8")?;
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| anyhow!("unexpected for-each-ref line {line:?}"))?;
        if !valid_oid(oid) {
            bail!("unexpected object id {oid:?} for {name:?}");
        }
        if !valid_ref_name(name) || !SNAPSHOT_PREFIXES.iter().any(|p| name.starts_with(p)) {
            bail!("unexpected ref name {name:?}");
        }
        refs.insert(name.to_string(), oid.to_string());
    }
    Ok(refs)
}

/// The parts of the shared configuration the private one copies.
#[derive(Debug, Default)]
struct SharedConfig {
    format_version: u32,
    object_format: Option<String>,
    booleans: Vec<(&'static str, bool)>,
}

/// Reads the repository format and the file system booleans from the shared repository's own
/// configuration (`--local`).
async fn shared_config(git: &SafeGit) -> Result<SharedConfig> {
    let output = run_git(
        git,
        &[
            "config",
            "--local",
            "--null",
            "--get-regexp",
            r"^(core\.(repositoryformatversion|filemode|symlinks|ignorecase|precomposeunicode)|extensions\.objectformat)$",
        ],
        &[1],
    )
    .await?;
    let mut config = SharedConfig::default();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let (key, value) = record.split_once('\n').unwrap_or((&record, ""));
        let key = key.to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "core.repositoryformatversion" => {
                config.format_version = value
                    .parse()
                    .with_context(|| format!("unexpected repository format version {value:?}"))?;
            }
            "extensions.objectformat" => {
                let value = value.to_ascii_lowercase();
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    bail!("unexpected object format {value:?}");
                }
                config.object_format = Some(value);
            }
            key => {
                let Some(name) = FILE_SYSTEM_BOOLEANS.iter().find(|name| **name == key) else {
                    continue;
                };
                let value = match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "1" => true,
                    "false" | "no" | "off" | "0" => false,
                    _ => continue,
                };
                config.booleans.retain(|(existing, _)| existing != name);
                config.booleans.push((name, value));
            }
        }
    }
    if config
        .object_format
        .as_deref()
        .is_some_and(|format| format != "sha1")
    {
        config.format_version = config.format_version.max(1);
    }
    Ok(config)
}

/// Makes `private_dir` afresh (see the module documentation). Every file is created new
/// (`create_new`), so nothing is written through a symlink in the last path component of a file;
/// directories on the way are trusted, which is why the scratch must not be job-controlled yet.
fn write_private_dir(
    private_dir: &Path,
    linked_git_dir: &Path,
    common_dir: &Path,
    branch: &str,
    snapshot: &BTreeMap<String, String>,
    config: &SharedConfig,
) -> Result<()> {
    remove_no_follow(private_dir)?;
    let dir = |relative: &str| {
        let path = private_dir.join(relative);
        fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))
    };
    dir("")?;
    dir("refs")?;
    dir("objects")?;
    dir("objects/info")?;
    dir("objects/pack")?;
    dir("info")?;

    write_new(
        &private_dir.join("HEAD"),
        format!("ref: {branch}\n").as_bytes(),
    )?;

    let mut text = format!(
        "[core]\n\trepositoryformatversion = {}\n\tbare = false\n\tlogallrefupdates = false\n",
        config.format_version
    );
    for (key, value) in &config.booleans {
        let name = key.trim_start_matches("core.");
        text.push_str(&format!("\t{name} = {value}\n"));
    }
    if let Some(format) = &config.object_format {
        text.push_str(&format!("[extensions]\n\tobjectformat = {format}\n"));
    }
    // An automatic gc would write a commit graph, reverse indexes and cruft packs, which the
    // import refuses; the job may still run one by hand.
    text.push_str("[gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n");
    write_new(&private_dir.join("config"), text.as_bytes())?;

    // No peeled lines, so git peels tags itself; the names are sorted bytewise (BTreeMap order).
    let mut packed = String::from("# pack-refs with: sorted \n");
    for (name, oid) in snapshot {
        packed.push_str(&format!("{oid} {name}\n"));
    }
    write_new(&private_dir.join("packed-refs"), packed.as_bytes())?;

    let objects = common_dir.join("objects");
    if !fs::symlink_metadata(&objects).is_ok_and(|metadata| metadata.is_dir()) {
        bail!(
            "the shared object directory {} is missing",
            objects.display()
        );
    }
    let mut alternates = line_bytes(&objects, "shared object directory")?.to_vec();
    alternates.push(b'\n');
    write_new(&private_dir.join("objects/info/alternates"), &alternates)?;

    copy_if_present(&linked_git_dir.join("index"), &private_dir.join("index"))?;
    // A split index keeps its shared part next to the index.
    let entries = fs::read_dir(linked_git_dir)
        .with_context(|| format!("reading {}", linked_git_dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", linked_git_dir.display()))?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("sharedindex."))
        {
            copy_if_present(&entry.path(), &private_dir.join(&name))?;
        }
    }
    copy_if_present(
        &common_dir.join("info/exclude"),
        &private_dir.join("info/exclude"),
    )?;
    copy_if_present(&common_dir.join("shallow"), &private_dir.join("shallow"))?;
    Ok(())
}

/// Copies the regular file `from` to the new file `to`; nothing when `from` does not exist.
fn copy_if_present(from: &Path, to: &Path) -> Result<()> {
    match fs::symlink_metadata(from) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => bail!("{} is not a regular file", from.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", from.display())),
    }
    let contents = fs::read(from).with_context(|| format!("reading {}", from.display()))?;
    write_new(to, &contents)
}

/// Writes `contents` to the new file `path`; fails when anything (a symlink too) is there.
fn write_new(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

/// Reads the regular file `path` through one open that refuses a symlink (on Unix); fails when
/// `path` is a symlink or anything but a regular file.
fn read_regular_no_follow(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;

    let describe = || format!("reading {}", path.display());
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // O_NONBLOCK so a FIFO planted at `path` cannot block the open; the regular-file check
        // below then refuses it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).with_context(describe)?;
    let metadata = file.metadata().with_context(describe)?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut bytes = Vec::new();
    file.take(DOT_GIT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .with_context(describe)?;
    if bytes.len() as u64 > DOT_GIT_LIMIT {
        bail!("{} is larger than {DOT_GIT_LIMIT} bytes", path.display());
    }
    Ok(bytes)
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The counter value the next temporary name will use.
#[cfg(test)]
pub(crate) fn next_temporary_counter() -> u64 {
    NEXT.load(Ordering::Relaxed)
}

/// A name for a temporary file next to `path`, unique in this process.
fn temporary_name(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let mut temporary = std::ffi::OsString::from(".");
    temporary.push(name);
    temporary.push(format!(
        ".bongos-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(temporary))
}

/// Replaces `path` with a file holding `contents`: written to a new temporary file next to it,
/// then renamed over it, so a symlink at `path` is replaced, never followed.
fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = temporary_name(path)?;
    // The name is predictable: whatever is at it (a symlink too) is removed, never followed.
    remove_no_follow(&temporary)?;
    write_new(&temporary, contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        remove_temporary(&temporary);
        return Err(error).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

fn remove_temporary(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        log::error!(
            "private git: cannot remove the temporary file {}: {error}",
            path.display()
        );
    }
}

/// Removes `path` without following a symlink: a symlink or file is unlinked, a directory is
/// removed with its contents (a symlink inside is unlinked, its target untouched).
fn remove_no_follow(path: &Path) -> Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Restores the tree's `.git`, removes the private directory and a stale import directory, then
/// the marker, in that order, so an undo cut short is finished by the next one. Each step is
/// skipped when already done; a tree that no longer exists is not recreated.
fn undo(
    linked_git_dir: &Path,
    tree: &Path,
    private_dir: &Path,
    original_dot_git: &[u8],
) -> Result<()> {
    restore_dot_git(tree, original_dot_git)?;
    if !private_dir.is_absolute()
        || private_dir.file_name() != Some(std::ffi::OsStr::new(PRIVATE_DIR))
    {
        bail!(
            "refusing to remove {}: not a private git directory",
            private_dir.display()
        );
    }
    remove_no_follow(private_dir)?;
    remove_no_follow(&linked_git_dir.join(import::IMPORT_DIR))?;
    remove_marker(linked_git_dir)
}

/// Puts the tree's `.git` back to `original_dot_git` (by an atomic rename) unless it already
/// holds it; nothing when the tree no longer exists.
fn restore_dot_git(tree: &Path, original_dot_git: &[u8]) -> Result<()> {
    match fs::symlink_metadata(tree) {
        Ok(metadata) if metadata.is_dir() => {
            let dot_git = tree.join(".git");
            let restored =
                read_regular_no_follow(&dot_git).is_ok_and(|contents| contents == original_dot_git);
            if !restored {
                // A directory the job put there is junk inside the tree: it cannot be renamed
                // over, so it goes first (the original text is in the marker).
                if fs::symlink_metadata(&dot_git).is_ok_and(|metadata| metadata.is_dir()) {
                    remove_no_follow(&dot_git)?;
                }
                replace_file(&dot_git, original_dot_git)?;
            }
        }
        Ok(_) => bail!("the tree {} is not a directory", tree.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", tree.display())),
    }
    Ok(())
}

/// Removes the marker of `linked_git_dir`, releasing the tree.
fn remove_marker(linked_git_dir: &Path) -> Result<()> {
    let marker = linked_git_dir.join(MARKER);
    match fs::remove_file(&marker) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", marker.display()))
        }
        _ => Ok(()),
    }
}

/// What the marker records (see the module documentation).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Marker {
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
    pub(crate) tree: PathBuf,
    pub(crate) private_dir: PathBuf,
    pub(crate) original_dot_git: Vec<u8>,
    /// Set once the snapshot is taken.
    pub(crate) branch: Option<String>,
    pub(crate) snapshot: Option<BTreeMap<String, String>>,
}

impl Marker {
    fn undo(&self, linked_git_dir: &Path) -> Result<()> {
        undo(
            linked_git_dir,
            &self.tree,
            &self.private_dir,
            &self.original_dot_git,
        )
    }

    /// The marker as text: a header, one `key value` line per field (paths and the `.git` text
    /// in hex), the snapshot when taken, and `end`.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut text = format!(
            "{MARKER_HEADER}\npid {}\nstart-time {}\ntree {}\nprivate-dir {}\ndot-git {}\n",
            self.pid,
            self.start_time,
            hex(self.tree.as_os_str().as_encoded_bytes()),
            hex(self.private_dir.as_os_str().as_encoded_bytes()),
            hex(&self.original_dot_git),
        );
        if let Some(branch) = &self.branch {
            text.push_str(&format!("branch {branch}\n"));
        }
        if let Some(snapshot) = &self.snapshot {
            text.push_str("snapshot\n");
            for (name, oid) in snapshot {
                text.push_str(&format!("ref {name} {oid}\n"));
            }
        }
        text.push_str("end\n");
        text.into_bytes()
    }

    /// Parses [`Marker::encode`]'s text; anything else (a missing `end`, an unknown line) fails.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).context("it is not text")?;
        let body = text
            .strip_prefix(MARKER_HEADER)
            .and_then(|rest| rest.strip_prefix('\n'))
            .ok_or_else(|| anyhow!("it lacks the header line"))?;
        let body = body
            .strip_suffix("end\n")
            .ok_or_else(|| anyhow!("it does not end with an end line"))?;
        let (mut pid, mut start_time, mut tree, mut private_dir, mut dot_git) =
            (None, None, None, None, None);
        let mut branch = None;
        let mut snapshot: Option<BTreeMap<String, String>> = None;
        for line in body.lines() {
            if line == "snapshot" {
                if snapshot.is_some() {
                    bail!("it has two snapshot lines");
                }
                snapshot = Some(BTreeMap::new());
                continue;
            }
            let (key, value) = line
                .split_once(' ')
                .ok_or_else(|| anyhow!("unexpected line {line:?}"))?;
            match key {
                "pid" => pid = Some(value.parse::<u32>().context("bad pid")?),
                "start-time" => start_time = Some(value.parse::<u64>().context("bad start time")?),
                "tree" => tree = Some(path_from_hex(value)?),
                "private-dir" => private_dir = Some(path_from_hex(value)?),
                "dot-git" => dot_git = Some(unhex(value)?),
                "branch" if valid_ref_name(value) => branch = Some(value.to_string()),
                "ref" => {
                    let refs = snapshot
                        .as_mut()
                        .ok_or_else(|| anyhow!("a ref line before the snapshot line"))?;
                    let (name, oid) = value
                        .split_once(' ')
                        .filter(|(name, oid)| valid_ref_name(name) && valid_oid(oid))
                        .ok_or_else(|| anyhow!("unexpected ref line {line:?}"))?;
                    refs.insert(name.to_string(), oid.to_string());
                }
                _ => bail!("unexpected line {line:?}"),
            }
        }
        let missing = |field: &str| anyhow!("it lacks the {field} line");
        Ok(Self {
            pid: pid.ok_or_else(|| missing("pid"))?,
            start_time: start_time.ok_or_else(|| missing("start-time"))?,
            tree: tree.ok_or_else(|| missing("tree"))?,
            private_dir: private_dir.ok_or_else(|| missing("private-dir"))?,
            original_dot_git: dot_git.ok_or_else(|| missing("dot-git"))?,
            branch,
            snapshot,
        })
    }

    /// The marker of `linked_git_dir`, or `None` when there is none. A marker that is not a
    /// regular file or cannot be parsed fails (see the module documentation).
    pub(crate) fn read(linked_git_dir: &Path) -> Result<Option<Self>> {
        let path = linked_git_dir.join(MARKER);
        let refuse = |reason: String| {
            anyhow!(
                "cannot read the marker {}: {reason}; nothing was changed. Remove it by hand once \
                 no job uses the tree",
                path.display()
            )
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(refuse(error.to_string())),
        };
        if !metadata.is_file() {
            return Err(refuse("it is not a regular file".to_string()));
        }
        if metadata.len() > MARKER_LIMIT {
            return Err(refuse(format!("it is larger than {MARKER_LIMIT} bytes")));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(refuse(error.to_string())),
        };
        Self::decode(&bytes)
            .map(Some)
            .map_err(|error| refuse(format!("{error:#}")))
    }

    /// Creates the marker whole: writes a temporary file, then hard-links it to the marker's
    /// name, which fails with [`BUSY`] when a marker is there.
    pub(crate) fn create(&self, linked_git_dir: &Path) -> Result<()> {
        let path = linked_git_dir.join(MARKER);
        let temporary = temporary_name(&path)?;
        write_new(&temporary, &self.encode())?;
        let linked = fs::hard_link(&temporary, &path);
        remove_temporary(&temporary);
        match linked {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                bail!("{BUSY}: {}", self.tree.display())
            }
            Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
        }
    }

    /// Replaces the marker with this one in one rename.
    pub(crate) fn replace(&self, linked_git_dir: &Path) -> Result<()> {
        replace_file(&linked_git_dir.join(MARKER), &self.encode())
    }
}

/// The advisory lock on a linked git directory's marker; released when dropped.
struct TreeLock {
    _file: fs::File,
}

impl TreeLock {
    /// Takes the lock, waiting for another holder. Only for the marker's owner: every other
    /// holder then only reads the marker, finds its owner alive and lets go.
    fn take(linked_git_dir: &Path) -> Result<Self> {
        let path = linked_git_dir.join(LOCK);
        let file = Self::open(&path)?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
    }

    fn open(path: &Path) -> Result<fs::File> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }

    /// Takes the lock, or `None` when another holder has it.
    fn try_take(linked_git_dir: &Path) -> Result<Option<Self>> {
        let path = linked_git_dir.join(LOCK);
        let file = Self::open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

/// This process as a marker's owner: its pid and start time (0 where start times are unknown).
fn this_owner() -> Result<(u32, u64)> {
    let pid = std::process::id();
    let start_time = util::job_roots::process_start_time(pid);
    if cfg!(target_os = "linux") && start_time.is_none() {
        bail!("cannot read the start time of this process ({pid})");
    }
    Ok((pid, start_time.unwrap_or_default()))
}

/// Whether the owner `pid`, started at `start_time`, is alive (see the module documentation).
/// When that cannot be told the owner counts as alive, so its tree stays taken.
pub(crate) fn owner_alive(pid: u32, start_time: u64) -> bool {
    if pid == 0 || i32::try_from(pid).is_err() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        util::job_roots::process_alive(pid, start_time)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = start_time;
        // SAFETY: signal 0 sends nothing; it only checks that the process exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = start_time;
        true
    }
}

fn canonical(path: &Path, what: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("the {what} must be an absolute path: {}", path.display());
    }
    fs::canonicalize(path).with_context(|| format!("resolving the {what} {}", path.display()))
}

/// The common directory the linked git directory's `commondir` names, canonical.
fn common_dir(linked_git_dir: &Path) -> Result<PathBuf> {
    let file = linked_git_dir.join("commondir");
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let target = text.trim_end_matches(['\n', '\r']);
    if target.is_empty() {
        bail!("{} is empty", file.display());
    }
    canonical(&linked_git_dir.join(target), "common git directory")
}

/// The bytes of `path`, refused when they hold a newline (a git pointer file holds one line).
fn line_bytes<'a>(path: &'a Path, what: &str) -> Result<&'a [u8]> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        bail!("the {what} {} holds a line break", path.display());
    }
    Ok(bytes)
}

/// A ref name as git writes them: under `refs/`, no space, control character or `\`.
fn valid_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && name.len() > "refs/".len()
        && name
            .bytes()
            .all(|byte| byte > b' ' && byte != 0x7f && byte != b'\\')
}

/// A full hexadecimal object id (SHA-1 or SHA-256).
fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("odd hex length");
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            text.get(index..index + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| anyhow!("bad hex"))
        })
        .collect()
}

fn path_from_hex(text: &str) -> Result<PathBuf> {
    let bytes = unhex(text)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            String::from_utf8(bytes).context("a path that is not UTF-8")?,
        ))
    }
}
