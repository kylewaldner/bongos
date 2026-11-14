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
}
