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
        let shown = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
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
        let output = command.output().await.map_err(|error| GitError::Spawn {
            args: shown.clone(),
            message: error.to_string(),
        })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

    /// The commit id `revision` names.
    pub async fn rev_parse(&self, directory: &Path, revision: &str) -> Result<String, GitError> {
        self.run(directory, &["rev-parse", "--verify", revision])
            .await
    }
}
