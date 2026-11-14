//! A runner over the `git` binary. Every call runs with `GIT_TERMINAL_PROMPT=0`, so a missing
//! credential fails instead of waiting for input.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The message of [`GitError::GitMissing`].
const GIT_MISSING: &str =
    "git is not installed (needed for project repositories); install git and try again";

pub const DEFAULT_IDENTITY_NAME: &str = "bongos";
pub const DEFAULT_IDENTITY_EMAIL: &str = "bongos@localhost";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed (needed for project repositories); install git and try again")]
    GitMissing,
    #[error("running git {args}: {message}")]
    Spawn { args: String, message: String },
    #[error("git {args} failed: {stderr}")]
    Failed { args: String, stderr: String },
}

impl GitError {
    /// The text a person sees when git is missing.
    pub fn missing_message() -> &'static str {
        GIT_MISSING
    }
}

/// Who a commit made by bongos is by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            name: DEFAULT_IDENTITY_NAME.to_string(),
            email: DEFAULT_IDENTITY_EMAIL.to_string(),
        }
    }
}

/// A commit id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitId(pub String);

/// A tree id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreeId(pub String);

/// The tree to read: a commit's, or a tree object directly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TreeRef {
    Commit(CommitId),
    Tree(TreeId),
}

impl TreeRef {
    /// The object name git resolves to the tree.
    pub fn treeish(&self) -> &str {
        match self {
            TreeRef::Commit(CommitId(id)) | TreeRef::Tree(TreeId(id)) => id,
        }
    }
}

/// One file of a tree, as `git ls-tree -r` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    /// `blob`, or `commit` for a submodule.
    pub kind: String,
    pub object: String,
    /// Relative to the tree's root, `/`-separated.
    pub path: String,
}

/// A located git binary.
#[derive(Clone, Debug)]
pub struct Git {
    binary: PathBuf,
    isolated: bool,
}

impl Git {
    /// Finds git on the `PATH` of this process.
    pub fn locate() -> Result<Self, GitError> {
        which::which("git")
            .map(Self::at)
            .map_err(|_| GitError::GitMissing)
    }

    /// Finds git in the directories of `path`, a `PATH`-style list. Lets a caller look up git
    /// without reading or changing the environment of the process.
    pub fn locate_in(path: &OsStr) -> Result<Self, GitError> {
        which::which_in("git", Some(path), ".")
            .map(Self::at)
            .map_err(|_| GitError::GitMissing)
    }

    /// A runner for the binary at `binary`.
    pub fn at(binary: PathBuf) -> Self {
        Self {
            binary,
            isolated: false,
        }
    }

    /// Ignores the user's and the system's git configuration, for tests.
    pub fn isolated(mut self) -> Self {
        self.isolated = true;
        self
    }

    /// Runs git with `args` in `directory` and returns its trimmed output.
    pub async fn run(&self, directory: &Path, args: &[&str]) -> Result<String, GitError> {
        self.run_os(directory, args.iter().map(OsString::from).collect())
            .await
    }

    async fn run_os(&self, directory: &Path, args: Vec<OsString>) -> Result<String, GitError> {
        let output = self.run_bytes(directory, args, None).await?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    /// Runs git with `args` in `directory`, feeding it `stdin`, and returns its raw output.
    async fn run_bytes(
        &self,
        directory: &Path,
        args: Vec<OsString>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, GitError> {
        self.run_bytes_with_index(directory, None, args, stdin)
            .await
    }

    /// [`Self::run_bytes`] with `GIT_INDEX_FILE` set to `index`, when given.
    async fn run_bytes_with_index(
        &self,
        directory: &Path,
        index: Option<&Path>,
        args: Vec<OsString>,
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, GitError> {
        use futures::AsyncWriteExt as _;

        let shown = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let spawn_error = |error: std::io::Error| GitError::Spawn {
            args: shown.clone(),
            message: error.to_string(),
        };
        let mut command = util::command::new_command(&self.binary);
        command
            .current_dir(directory)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        if self.isolated {
            command
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
        let output = match stdin {
            None => command.output().await.map_err(spawn_error)?,
            Some(input) => {
                command
                    .stdin(util::command::Stdio::piped())
                    .stdout(util::command::Stdio::piped())
                    .stderr(util::command::Stdio::piped());
                let mut child = command.spawn().map_err(spawn_error)?;
                let mut pipe = child.stdin.take();
                // Written while the output is read, so a large answer never fills its pipe and
                // stalls the writer.
                let feed = async move {
                    if let Some(pipe) = pipe.as_mut() {
                        pipe.write_all(&input).await?;
                        pipe.close().await?;
                    }
                    drop(pipe);
                    Ok::<(), std::io::Error>(())
                };
                let (fed, output) = futures::join!(feed, child.output());
                let output = output.map_err(spawn_error)?;
                if output.status.success() {
                    fed.map_err(spawn_error)?;
                }
                output
            }
        };
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(GitError::Failed {
                args: shown,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    /// Makes `directory` (which must exist) a repository whose first branch is `branch`.
    pub async fn init_repo(&self, directory: &Path, branch: &str) -> Result<(), GitError> {
        self.run(directory, &["init", "-q", "-b", branch]).await?;
        Ok(())
    }

    /// Stages everything in `directory` and commits it, returning the new commit's id.
    pub async fn commit_all(
        &self,
        directory: &Path,
        message: &str,
        identity: &Identity,
    ) -> Result<String, GitError> {
        self.run(directory, &["add", "-A"]).await?;
        let args = [
            "-c".to_string(),
            format!("user.name={}", identity.name),
            "-c".to_string(),
            format!("user.email={}", identity.email),
            "-c".to_string(),
            "commit.gpgsign=false".to_string(),
            "commit".to_string(),
            "-q".to_string(),
            "--allow-empty".to_string(),
            "-m".to_string(),
            message.to_string(),
        ];
        self.run_os(directory, args.into_iter().map(OsString::from).collect())
            .await?;
        self.rev_parse(directory, "HEAD").await
    }

    /// Every file of the tree `treeish` names, recursively, in one `git ls-tree -r`.
    pub async fn ls_tree_r(
        &self,
        directory: &Path,
        treeish: &str,
    ) -> Result<Vec<TreeEntry>, GitError> {
        let args = ["ls-tree", "-r", "-z", "--full-tree", treeish]
            .into_iter()
            .map(OsString::from)
            .collect();
        let output = self.run_bytes(directory, args, None).await?;
        let mut entries = Vec::new();
        for record in output.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let record = String::from_utf8_lossy(record);
            let Some((meta, path)) = record.split_once('\t') else {
                continue;
            };
            let mut parts = meta.split(' ');
            let (Some(mode), Some(kind), Some(object)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            entries.push(TreeEntry {
                mode: mode.to_string(),
                kind: kind.to_string(),
                object: object.to_string(),
                path: path.to_string(),
            });
        }
        Ok(entries)
    }

    /// The contents of each object in `objects` (object ids, or `<commit>:<path>`), in order,
    /// from one `git cat-file --batch`. A missing object is `None`.
    pub async fn cat_file_batch(
        &self,
        directory: &Path,
        objects: &[String],
    ) -> Result<Vec<Option<Vec<u8>>>, GitError> {
        if objects.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for object in objects {
            input.extend_from_slice(object.as_bytes());
            input.push(b'\n');
        }
        let args = vec![OsString::from("cat-file"), OsString::from("--batch")];
        let output = self.run_bytes(directory, args, Some(input)).await?;
        let malformed = |what: &str| GitError::Failed {
            args: "cat-file --batch".to_string(),
            stderr: format!("unexpected output: {what}"),
        };
        let mut contents = Vec::with_capacity(objects.len());
        let mut rest = output.as_slice();
        for _ in objects {
            let newline = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(|| malformed("a header without a newline"))?;
            let header = String::from_utf8_lossy(&rest[..newline]).to_string();
            rest = &rest[newline + 1..];
            if header.ends_with(" missing") || header.ends_with(" ambiguous") {
                contents.push(None);
                continue;
            }
            let size: usize = header
                .rsplit(' ')
                .next()
                .and_then(|size| size.parse().ok())
                .ok_or_else(|| malformed(&header))?;
            if rest.len() < size + 1 {
                return Err(malformed("a short object"));
            }
            contents.push(Some(rest[..size].to_vec()));
            rest = &rest[size + 1..];
        }
        Ok(contents)
    }

    /// The commit id `revision` names.
    pub async fn rev_parse(&self, directory: &Path, revision: &str) -> Result<String, GitError> {
        self.run(directory, &["rev-parse", "--verify", revision])
            .await
    }
    /// Adds a worktree at `path` on the new branch `branch`, made from `commit`.
    pub async fn worktree_add(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
        commit: &str,
    ) -> Result<(), GitError> {
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-q"),
            OsString::from("-b"),
            OsString::from(branch),
            path.as_os_str().to_owned(),
            OsString::from(commit),
        ];
        self.run_os(repository, args).await?;
        Ok(())
    }

    /// Adds a worktree at `path` with the existing branch `branch` checked out.
    pub async fn worktree_checkout(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<(), GitError> {
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-q"),
            path.as_os_str().to_owned(),
            OsString::from(branch),
        ];
        self.run_os(repository, args).await?;
        Ok(())
    }

    /// Runs git with `args` in `directory` against the index file `index` instead of the
    /// repository's, feeding it `stdin`, and returns its raw output.
    pub async fn run_on_index(
        &self,
        directory: &Path,
        index: &Path,
        args: &[&str],
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, GitError> {
        let args = args.iter().map(OsString::from).collect();
        self.run_bytes_with_index(directory, Some(index), args, stdin)
            .await
    }

    /// Writes each of `files` (paths of files on disk) as a blob and returns their ids, in order.
    pub async fn hash_objects(
        &self,
        directory: &Path,
        files: &[PathBuf],
    ) -> Result<Vec<String>, GitError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for file in files {
            input.extend_from_slice(file.as_os_str().as_encoded_bytes());
            input.push(b'\n');
        }
        let args = ["hash-object", "-w", "--no-filters", "--stdin-paths"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let output = self.run_bytes(directory, args, Some(input)).await?;
        Ok(String::from_utf8_lossy(&output)
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Adds a worktree at `path` with `commit` checked out and no branch.
    pub async fn worktree_add_detached(
        &self,
        repository: &Path,
        path: &Path,
        commit: &str,
    ) -> Result<(), GitError> {
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-q"),
            OsString::from("--detach"),
            path.as_os_str().to_owned(),
            OsString::from(commit),
        ];
        self.run_os(repository, args).await?;
        Ok(())
    }

    /// Removes the worktree at `path`, with whatever is uncommitted in it, and forgets worktrees
    /// whose folder is gone.
    pub async fn worktree_remove(&self, repository: &Path, path: &Path) -> Result<(), GitError> {
        let args = vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--force"),
            path.as_os_str().to_owned(),
        ];
        let removed = self.run_os(repository, args).await;
        self.run(repository, &["worktree", "prune"]).await?;
        match removed {
            Err(_) if !path.exists() => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Whether the local branch `branch` exists.
    pub async fn branch_exists(&self, repository: &Path, branch: &str) -> bool {
        let reference = format!("refs/heads/{branch}");
        self.run(repository, &["show-ref", "--verify", "--quiet", &reference])
            .await
            .is_ok()
    }

    /// Deletes the local branch `branch`, merged or not.
    pub async fn branch_delete(&self, repository: &Path, branch: &str) -> Result<(), GitError> {
        self.run(repository, &["branch", "-D", branch]).await?;
        Ok(())
    }

    /// Runs git with `identity` as author and committer and signing off.
    async fn run_as(
        &self,
        directory: &Path,
        identity: &Identity,
        args: &[&str],
    ) -> Result<String, GitError> {
        let mut all = vec![
            "-c".to_string(),
            format!("user.name={}", identity.name),
            "-c".to_string(),
            format!("user.email={}", identity.email),
            "-c".to_string(),
            "commit.gpgsign=false".to_string(),
            "-c".to_string(),
            "core.editor=true".to_string(),
        ];
        all.extend(args.iter().map(|arg| arg.to_string()));
        self.run_os(directory, all.into_iter().map(OsString::from).collect())
            .await
    }

    /// Whether the checkout at `directory` has no uncommitted change, untracked files included.
    pub async fn is_clean(&self, directory: &Path) -> Result<bool, GitError> {
        Ok(self
            .run(
                directory,
                &["status", "--porcelain", "--untracked-files=all"],
            )
            .await?
            .is_empty())
    }

    /// The tree of `commit`.
    pub async fn tree_of(&self, directory: &Path, commit: &str) -> Result<String, GitError> {
        self.rev_parse(directory, &format!("{commit}^{{tree}}"))
            .await
    }

    /// Makes a commit of `tree` with `parents` and `message` without touching any ref.
    pub async fn commit_tree(
        &self,
        directory: &Path,
        tree: &str,
        parents: &[&str],
        message: &str,
        identity: &Identity,
    ) -> Result<String, GitError> {
        let mut args = vec!["commit-tree", tree];
        for parent in parents {
            args.push("-p");
            args.push(parent);
        }
        args.push("-m");
        args.push(message);
        self.run_as(directory, identity, &args).await
    }

    /// Moves `reference` to `new` only while it is `old`. Fails, changing nothing, otherwise.
    pub async fn update_ref_cas(
        &self,
        directory: &Path,
        reference: &str,
        new: &str,
        old: &str,
    ) -> Result<(), GitError> {
        self.run(directory, &["update-ref", reference, new, old])
            .await?;
        Ok(())
    }

    /// Creates or moves `reference` to `commit`.
    pub async fn update_ref(
        &self,
        directory: &Path,
        reference: &str,
        commit: &str,
    ) -> Result<(), GitError> {
        self.run(directory, &["update-ref", reference, commit])
            .await?;
        Ok(())
    }

    /// Deletes `reference`.
    pub async fn delete_ref(&self, directory: &Path, reference: &str) -> Result<(), GitError> {
        self.run(directory, &["update-ref", "-d", reference])
            .await?;
        Ok(())
    }

    /// Brings the index and files of the checkout at `directory` to its `HEAD` after the branch
    /// moved forward under it, removing the files the new commit no longer has.
    pub async fn update_checkout(&self, directory: &Path) -> Result<(), GitError> {
        self.run(directory, &["read-tree", "--reset", "-u", "HEAD"])
            .await?;
        Ok(())
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors.
    pub async fn is_ancestor(&self, directory: &Path, ancestor: &str, descendant: &str) -> bool {
        self.run(
            directory,
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
        .await
        .is_ok()
    }

    /// The commits reachable from `to` and not from `from`, oldest first.
    pub async fn commits_between(
        &self,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<Vec<String>, GitError> {
        let range = format!("{from}..{to}");
        Ok(self
            .run(directory, &["rev-list", "--reverse", &range])
            .await?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// The full message of `commit`.
    pub async fn message_of(&self, directory: &Path, commit: &str) -> Result<String, GitError> {
        self.run(directory, &["log", "-1", "--format=%B", commit])
            .await
    }

    /// The parents of `commit`, in order.
    pub async fn parents_of(
        &self,
        directory: &Path,
        commit: &str,
    ) -> Result<Vec<String>, GitError> {
        let line = self
            .run(directory, &["rev-list", "--parents", "-n", "1", commit])
            .await?;
        Ok(line
            .split_whitespace()
            .skip(1)
            .map(str::to_string)
            .collect())
    }

    /// Commits what is uncommitted in the checkout at `directory` as `identity`; `None` when
    /// there was nothing to commit.
    pub async fn commit_if_dirty(
        &self,
        directory: &Path,
        message: &str,
        identity: &Identity,
    ) -> Result<Option<String>, GitError> {
        if self.is_clean(directory).await? {
            return Ok(None);
        }
        self.run(directory, &["add", "-A"]).await?;
        self.run_as(directory, identity, &["commit", "-q", "-m", message])
            .await?;
        Ok(Some(self.rev_parse(directory, "HEAD").await?))
    }

    /// The files with unmerged entries in the checkout at `directory`.
    pub async fn conflicted_files(&self, directory: &Path) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(directory, &["diff", "--name-only", "--diff-filter=U"])
            .await?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Whether a rebase is stopped in the checkout at `directory`.
    pub async fn rebase_in_progress(&self, directory: &Path) -> bool {
        for name in ["rebase-merge", "rebase-apply"] {
            if let Ok(path) = self
                .run(directory, &["rev-parse", "--git-path", name])
                .await
            {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                };
                if path.exists() {
                    return true;
                }
            }
        }
        false
    }

    /// Rebases the branch checked out at `directory` onto `onto`. A conflict leaves the rebase
    /// stopped, with markers in the files, for repair.
    pub async fn rebase(
        &self,
        directory: &Path,
        onto: &str,
        identity: &Identity,
    ) -> Result<RebaseOutcome, GitError> {
        self.rebase_args(directory, identity, &["rebase", "-q", onto])
            .await
    }

    /// Moves the commits after `upstream` up to `HEAD` onto `onto`, dropping those up to and
    /// including `upstream`.
    pub async fn rebase_onto(
        &self,
        directory: &Path,
        onto: &str,
        upstream: &str,
        identity: &Identity,
    ) -> Result<RebaseOutcome, GitError> {
        self.rebase_args(
            directory,
            identity,
            &["rebase", "-q", "--onto", onto, upstream],
        )
        .await
    }

    /// Continues a stopped rebase once its files are resolved and staged.
    pub async fn rebase_continue(
        &self,
        directory: &Path,
        identity: &Identity,
    ) -> Result<RebaseOutcome, GitError> {
        self.run(directory, &["add", "-A"]).await?;
        self.rebase_args(directory, identity, &["rebase", "--continue"])
            .await
    }

    async fn rebase_args(
        &self,
        directory: &Path,
        identity: &Identity,
        args: &[&str],
    ) -> Result<RebaseOutcome, GitError> {
        match self.run_as(directory, identity, args).await {
            Ok(_) => Ok(RebaseOutcome::Done),
            Err(error) => {
                let files = self.conflicted_files(directory).await.unwrap_or_default();
                if files.is_empty() {
                    Err(error)
                } else {
                    Ok(RebaseOutcome::Conflict { files })
                }
            }
        }
    }

    /// `git reset --keep commit`: moves the branch checked out at `directory` to `commit`,
    /// refusing when that would lose an uncommitted change.
    pub async fn reset_keep(&self, directory: &Path, commit: &str) -> Result<(), GitError> {
        self.run(directory, &["reset", "-q", "--keep", commit])
            .await?;
        Ok(())
    }

    /// `git merge --ff-only commit`: moves the branch checked out at `directory` forward to
    /// `commit`, refusing when it is not a fast-forward or a file in the way would be lost.
    pub async fn merge_ff_only(&self, directory: &Path, commit: &str) -> Result<(), GitError> {
        self.run(directory, &["merge", "-q", "--ff-only", commit])
            .await?;
        Ok(())
    }

    /// The binary diff from `from` to `to`.
    pub async fn diff_binary(
        &self,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<Vec<u8>, GitError> {
        let args = ["diff", "--binary", from, to]
            .into_iter()
            .map(OsString::from)
            .collect();
        self.run_bytes(directory, args, None).await
    }

    /// The text of the diff from `from` to `to`, as `git diff` shows it (no color, no external
    /// diff or text conversion).
    pub async fn diff_text(
        &self,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<String, GitError> {
        let args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let output = self.run_bytes(directory, args, None).await?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// The paths that differ between `from` and `to`, sorted.
    pub async fn diff_names(
        &self,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<Vec<String>, GitError> {
        let output = self
            .run(
                directory,
                &["diff", "--name-only", "--no-renames", from, to],
            )
            .await?;
        Ok(output.lines().map(str::to_string).collect())
    }

    /// The best common ancestor of `a` and `b`.
    pub async fn merge_base(&self, directory: &Path, a: &str, b: &str) -> Result<String, GitError> {
        self.run(directory, &["merge-base", a, b]).await
    }

    /// The id of the diff from `from` to `to` (binary contents included, renames off, no external
    /// diff or text conversion), or `"empty"` when they hold the same files: its stable
    /// `git patch-id`, then the SHA-256 of the diff without its hunk headers and `index` lines.
    /// `git patch-id` ignores whitespace inside a line; the digest does not, so `rm -rf /tmp/x`
    /// and `rm -rf / tmp/x` get different ids. Both stay the same when the two commits are
    /// rebased together onto another base without touching the lines around the changed ones.
    pub async fn patch_id(
        &self,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<String, GitError> {
        let args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--full-index",
            "--binary",
            from,
            to,
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let diff = self.run_bytes(directory, args, None).await?;
        if diff.is_empty() {
            return Ok("empty".to_string());
        }
        let args = ["patch-id", "--stable"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let digest = rebase_stable_digest(&diff);
        let output = self.run_bytes(directory, args, Some(diff)).await?;
        let text = String::from_utf8_lossy(&output);
        let patch_id = text.split_whitespace().next().unwrap_or("empty");
        Ok(format!("{patch_id}-{digest}"))
    }

    /// The tree of `git revert` of `commit` applied to `onto`, by a merge that touches no
    /// checkout; `None` when it conflicts.
    pub async fn revert_tree(
        &self,
        directory: &Path,
        commit: &str,
        onto: &str,
    ) -> Result<Option<String>, GitError> {
        let base = format!("--merge-base={commit}");
        let parent = format!("{commit}^");
        match self
            .run(
                directory,
                &["merge-tree", "--write-tree", &base, onto, &parent],
            )
            .await
        {
            Ok(output) => Ok(output.lines().next().map(str::to_string)),
            Err(GitError::Failed { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Applies `patch` to the files and index of the checkout at `directory`.
    pub async fn apply_index(&self, directory: &Path, patch: Vec<u8>) -> Result<(), GitError> {
        if patch.is_empty() {
            return Ok(());
        }
        let args = ["apply", "--index"]
            .into_iter()
            .map(OsString::from)
            .collect();
        self.run_bytes(directory, args, Some(patch)).await?;
        Ok(())
    }

    /// Pushes `refspec` to `remote` (a URL or a remote name).
    pub async fn push(
        &self,
        directory: &Path,
        remote: &str,
        refspec: &str,
    ) -> Result<(), GitError> {
        self.run(directory, &["push", "-q", remote, refspec])
            .await?;
        Ok(())
    }
}

/// How a rebase ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseOutcome {
    Done,
    /// Stopped on a textual conflict in `files`, left in place with markers.
    Conflict {
        files: Vec<String>,
    },
}

/// The hex SHA-256 of `diff` (a `git diff` without color) with each hunk header cut to `@@` and
/// each `index` line left out: the line numbers and blob ids are all a rebase changes in it.
/// A content line starts with a space, `+`, `-` or `\`, and a binary literal line with a length
/// letter, so neither is mistaken for one of those.
pub(crate) fn rebase_stable_digest(diff: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    for line in diff.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"index ") {
            continue;
        }
        if line.starts_with(b"@@") {
            hasher.update(b"@@\n");
        } else {
            hasher.update(line);
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
