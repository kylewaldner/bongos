//! Git operations for language extensions: resolving a revision of a repository to a commit and
//! fetching that commit into a staging folder.
//!
//! Only [`resolve`] and [`stage`] reach the network. Every `git` process runs with terminal prompts
//! off and no askpass helper, so a repository that wants credentials fails instead of hanging.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use bongos_extensions_model::{Layout, RevSpec, SourceUrl, validate_relative_path};
use util::ResultExt as _;
use util::command::{Stdio, new_command};

/// The environment every `git` process gets, on top of the inherited one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitEnv {
    /// Variables set to a value.
    pub set: Vec<(&'static str, &'static str)>,
    /// Variables removed.
    pub remove: Vec<&'static str>,
    /// Prefixes of variables removed: every inherited variable whose name starts with one of them.
    pub remove_prefixes: Vec<&'static str>,
}

/// The ssh command used when the caller chose none: never asks for a password, passphrase or
/// host-key confirmation.
pub const BATCH_SSH_COMMAND: &str = "ssh -o BatchMode=yes";

/// The environment policy shared by [`RealGit`] and the fake runner, for the current process
/// environment. See [`git_env_for`].
pub fn git_env() -> GitEnv {
    git_env_for(|key| std::env::var_os(key).is_some())
}

/// The environment policy given which variables the caller's environment has: no terminal prompt,
/// no askpass helper, no interactive credential manager, ssh in batch mode unless the caller chose
/// an ssh command, and no inherited variable that would select another repository or inject
/// configuration.
pub fn git_env_for(has_var: impl Fn(&str) -> bool) -> GitEnv {
    let mut set = vec![("GIT_TERMINAL_PROMPT", "0"), ("GCM_INTERACTIVE", "never")];
    if !has_var("GIT_SSH_COMMAND") && !has_var("GIT_SSH") {
        set.push(("GIT_SSH_COMMAND", BATCH_SSH_COMMAND));
    }
    GitEnv {
        set,
        remove: vec![
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ],
        remove_prefixes: vec!["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"],
    }
}

/// Options passed to every `git` invocation before the subcommand: only https, ssh and file
/// transports, and no filesystem monitor hook.
const HARDENING: [&str; 12] = [
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.ssh.allow=always",
    "-c",
    "protocol.file.allow=always",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.fsmonitor=false",
];

/// One line of `git ls-remote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// The full ref name, such as `HEAD`, `refs/heads/main` or `refs/tags/v1^{}`.
    pub name: String,
    /// The object id the ref points at.
    pub oid: String,
}

/// Runs git. [`RealGit`] runs the `git` binary; tests substitute a fake.
#[async_trait]
pub trait GitRunner: Send + Sync {
    /// Lists the refs of a remote repository. Reaches the network.
    async fn ls_remote(&self, url: &SourceUrl) -> Result<Vec<RemoteRef>>;
    /// Creates `dir` as a repository holding `commit` of `url`, checked out detached. Reaches the
    /// network.
    async fn fetch_commit(&self, url: &SourceUrl, commit: &str, dir: &Path) -> Result<()>;
    /// Clones the local repository `src` into `dir`.
    async fn clone_local(&self, src: &Path, dir: &Path) -> Result<()>;
    /// The `origin` URL of the repository in `dir`.
    async fn remote_url(&self, dir: &Path) -> Result<String>;
    /// The commit checked out in `dir`.
    async fn head(&self, dir: &Path) -> Result<String>;
}

/// Runs the `git` binary.
#[derive(Debug, Clone, Default)]
pub struct RealGit {
    extra_env: Vec<(OsString, OsString)>,
}

impl RealGit {
    pub fn new() -> Self {
        Self::default()
    }

    /// A runner that also sets `env` on every process, after the shared policy.
    pub fn with_env(env: Vec<(OsString, OsString)>) -> Self {
        Self { extra_env: env }
    }

    /// Runs `git <args>` and returns its standard output. The error carries git's message.
    async fn run(&self, cwd: Option<&Path>, args: &[&std::ffi::OsStr]) -> Result<String> {
        let mut command = new_command("git");
        let policy = git_env();
        for key in policy.remove {
            command.env_remove(key);
        }
        for (key, _) in std::env::vars_os() {
            let matches = key.to_str().is_some_and(|key| {
                policy
                    .remove_prefixes
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
            });
            if matches {
                command.env_remove(key);
            }
        }
        for (key, value) in policy.set {
            command.env(key, value);
        }
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .args(HARDENING)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = command.output().await.context("could not run git")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            if message.is_empty() {
                bail!("git exited with {}", output.status);
            }
            bail!("{message}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn os(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

/// Parses `git ls-remote` output, skipping `--symref` lines.
pub fn parse_ls_remote(output: &str) -> Vec<RemoteRef> {
    output
        .lines()
        .filter(|line| !line.starts_with("ref: "))
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some(RemoteRef {
                name: name.trim().to_string(),
                oid: oid.trim().to_string(),
            })
        })
        .collect()
}

fn check_commit(commit: &str) -> Result<()> {
    if matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(anyhow!("`{commit}` is not a full commit hash"))
    }
}

#[async_trait]
impl GitRunner for RealGit {
    async fn ls_remote(&self, url: &SourceUrl) -> Result<Vec<RemoteRef>> {
        let output = self
            .run(
                None,
                &[os("ls-remote"), os("--symref"), os("--"), os(url.as_str())],
            )
            .await?;
        Ok(parse_ls_remote(&output))
    }

    async fn fetch_commit(&self, url: &SourceUrl, commit: &str, dir: &Path) -> Result<()> {
        check_commit(commit)?;
        self.run(
            None,
            &[os("init"), os("--quiet"), os("--"), dir.as_os_str()],
        )
        .await?;
        self.run(
            Some(dir),
            &[
                os("remote"),
                os("add"),
                os("--"),
                os("origin"),
                os(url.as_str()),
            ],
        )
        .await?;
        let shallow = self
            .run(
                Some(dir),
                &[
                    os("fetch"),
                    os("--quiet"),
                    os("--no-tags"),
                    os("--depth"),
                    os("1"),
                    os("origin"),
                    os(commit),
                ],
            )
            .await;
        if let Err(shallow_err) = shallow {
            // The server refuses unadvertised commits: fetch everything and look for it.
            self.run(Some(dir), &[os("fetch"), os("--quiet"), os("origin")])
                .await
                .map_err(|full_err| {
                    anyhow!(
                        "the shallow fetch failed ({shallow_err:#}) and so did the full fetch ({full_err:#})"
                    )
                })?;
            self.run(
                Some(dir),
                &[os("checkout"), os("--quiet"), os("--detach"), os(commit)],
            )
            .await?;
        } else {
            self.run(
                Some(dir),
                &[
                    os("checkout"),
                    os("--quiet"),
                    os("--detach"),
                    os("FETCH_HEAD"),
                ],
            )
            .await?;
        }
        Ok(())
    }

    async fn clone_local(&self, src: &Path, dir: &Path) -> Result<()> {
        self.run(
            None,
            &[
                os("clone"),
                os("--quiet"),
                os("--"),
                src.as_os_str(),
                dir.as_os_str(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn remote_url(&self, dir: &Path) -> Result<String> {
        let output = self
            .run(Some(dir), &[os("remote"), os("get-url"), os("origin")])
            .await?;
        Ok(output.trim().to_string())
    }

    async fn head(&self, dir: &Path) -> Result<String> {
        let output = self
            .run(
                Some(dir),
                &[os("rev-parse"), os("--verify"), os("HEAD^{commit}")],
            )
            .await?;
        Ok(output.trim().to_string())
    }
}

/// Resolves `rev` of `url` to a commit. A commit is used as given without reaching the network; a
/// name is looked up as a tag first (the peeled commit of an annotated tag), then as a branch; no
/// revision means the remote's default branch.
pub async fn resolve(runner: &dyn GitRunner, url: &SourceUrl, rev: &RevSpec) -> Result<String> {
    let wanted: Vec<String> = match rev {
        RevSpec::Commit(commit) => return Ok(commit.clone()),
        RevSpec::Name(name) => vec![
            format!("refs/tags/{name}^{{}}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
        ],
        RevSpec::Default => vec!["HEAD".to_string()],
    };
    let refs = runner
        .ls_remote(url)
        .await
        .map_err(|err| anyhow!("could not list the refs of {url}: {err:#}"))?;
    wanted
        .iter()
        .find_map(|name| refs.iter().find(|r| &r.name == name))
        .map(|r| r.oid.clone())
        .ok_or_else(|| match rev {
            RevSpec::Name(name) => anyhow!("no tag or branch `{name}` in {url}"),
            _ => anyhow!("{url} has no default branch (no HEAD)"),
        })
}

/// A fetched commit in a staging folder. Dropping it deletes the folder unless [`keep`] was called.
///
/// [`keep`]: StagedClone::keep
#[derive(Debug)]
pub struct StagedClone {
    dir: PathBuf,
    root: PathBuf,
    commit: String,
    armed: bool,
}

impl StagedClone {
    /// The staging folder, the root of the checkout.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The extension's root: the subdirectory given to [`stage`], or the checkout root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The commit checked out.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Keeps the staging folder on disk and returns it.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.dir)
    }
}

impl Drop for StagedClone {
    fn drop(&mut self) {
        if self.armed {
            std::fs::remove_dir_all(&self.dir).log_err();
        }
    }
}

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A staging folder name unique within the process and unlikely to collide across processes.
fn new_nonce() -> String {
    let count = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(count);
    hasher.write_u32(std::process::id());
    format!(
        "{:x}-{count:x}-{:016x}",
        std::process::id(),
        hasher.finish()
    )
}

/// Fetches `commit` of `url` into a new folder under the staging directory, creating `git/` and
/// `git/staging/` on first use. On failure the folder is deleted.
pub async fn stage(
    runner: &dyn GitRunner,
    layout: &Layout,
    url: &SourceUrl,
    commit: &str,
    path: Option<&str>,
) -> Result<StagedClone> {
    if let Some(path) = path {
        validate_relative_path(path)?;
    }
    let staging = layout.staging_dir();
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("could not create {}", staging.display()))?;
    let dir = loop {
        let dir = layout.staging_for(&new_nonce());
        match std::fs::create_dir(&dir) {
            Ok(()) => break dir,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("could not create {}", dir.display()));
            }
        }
    };
    let root = match path {
        Some(path) => dir.join(path),
        None => dir.clone(),
    };
    let staged = StagedClone {
        dir,
        root,
        commit: commit.to_string(),
        armed: true,
    };
    runner
        .fetch_commit(url, commit, &staged.dir)
        .await
        .map_err(|err| anyhow!("could not fetch {commit} from {url}: {err:#}"))?;
    if let Some(path) = path {
        // A committed symlink could point the subdirectory outside the checkout.
        let inside = match (staged.dir.canonicalize(), staged.root.canonicalize()) {
            (Ok(dir), Ok(root)) => root.starts_with(&dir) && root.is_dir(),
            _ => false,
        };
        if !inside {
            bail!("`{path}` of {url} at {commit} is not a directory inside the repository");
        }
    }
    Ok(staged)
}

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeGit;

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use bongos_extensions_model::SourceUrl;
    use parking_lot::Mutex;

    use super::{GitEnv, GitRunner, RemoteRef, git_env};

    #[derive(Default)]
    struct State {
        refs: HashMap<String, Vec<RemoteRef>>,
        ls_remote_failures: HashMap<String, Vec<String>>,
        fetch_failures: HashMap<String, Vec<String>>,
        checkouts: HashMap<PathBuf, (String, String)>,
        network_ops: usize,
        envs: Vec<GitEnv>,
    }

    /// A scriptable [`GitRunner`] that never runs git.
    #[derive(Default)]
    pub struct FakeGit {
        state: Mutex<State>,
    }

    impl FakeGit {
        pub fn new() -> Self {
            Self::default()
        }

        /// Sets the refs `ls_remote` returns for `url`, as `(name, oid)` pairs.
        pub fn set_refs(&self, url: &SourceUrl, refs: &[(&str, &str)]) {
            self.state.lock().refs.insert(
                url.as_str().to_string(),
                refs.iter()
                    .map(|(name, oid)| RemoteRef {
                        name: name.to_string(),
                        oid: oid.to_string(),
                    })
                    .collect(),
            );
        }

        /// Makes the next `ls_remote` of `url` fail with `message`.
        pub fn fail_next_ls_remote(&self, url: &SourceUrl, message: &str) {
            self.state
                .lock()
                .ls_remote_failures
                .entry(url.as_str().to_string())
                .or_default()
                .push(message.to_string());
        }

        /// Makes the next `fetch_commit` of `url` fail with `message`.
        pub fn fail_next_fetch(&self, url: &SourceUrl, message: &str) {
            self.state
                .lock()
                .fetch_failures
                .entry(url.as_str().to_string())
                .or_default()
                .push(message.to_string());
        }

        /// How many network operations (`ls_remote` and `fetch_commit`) ran.
        pub fn network_ops(&self) -> usize {
            self.state.lock().network_ops
        }

        /// The environment [`super::RealGit`] would have used, one entry per operation.
        pub fn recorded_envs(&self) -> Vec<GitEnv> {
            self.state.lock().envs.clone()
        }

        fn record(&self, network: bool) {
            let mut state = self.state.lock();
            state.envs.push(git_env());
            if network {
                state.network_ops += 1;
            }
        }
    }

    fn pop_failure(failures: &mut HashMap<String, Vec<String>>, url: &str) -> Option<String> {
        let queue = failures.get_mut(url)?;
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    #[async_trait]
    impl GitRunner for FakeGit {
        async fn ls_remote(&self, url: &SourceUrl) -> Result<Vec<RemoteRef>> {
            self.record(true);
            let mut state = self.state.lock();
            if let Some(message) = pop_failure(&mut state.ls_remote_failures, url.as_str()) {
                return Err(anyhow!(message));
            }
            state
                .refs
                .get(url.as_str())
                .cloned()
                .ok_or_else(|| anyhow!("repository '{url}' not found"))
        }

        async fn fetch_commit(&self, url: &SourceUrl, commit: &str, dir: &Path) -> Result<()> {
            self.record(true);
            let mut state = self.state.lock();
            if let Some(message) = pop_failure(&mut state.fetch_failures, url.as_str()) {
                return Err(anyhow!(message));
            }
            std::fs::create_dir_all(dir)?;
            state.checkouts.insert(
                dir.to_path_buf(),
                (url.as_str().to_string(), commit.to_string()),
            );
            Ok(())
        }

        async fn clone_local(&self, src: &Path, dir: &Path) -> Result<()> {
            self.record(false);
            let mut state = self.state.lock();
            let entry = state
                .checkouts
                .get(src)
                .cloned()
                .ok_or_else(|| anyhow!("{} is not a repository", src.display()))?;
            std::fs::create_dir_all(dir)?;
            state.checkouts.insert(dir.to_path_buf(), entry);
            Ok(())
        }

        async fn remote_url(&self, dir: &Path) -> Result<String> {
            self.record(false);
            self.state
                .lock()
                .checkouts
                .get(dir)
                .map(|(url, _)| url.clone())
                .ok_or_else(|| anyhow!("{} is not a repository", dir.display()))
        }

        async fn head(&self, dir: &Path) -> Result<String> {
            self.record(false);
            self.state
                .lock()
                .checkouts
                .get(dir)
                .map(|(_, commit)| commit.clone())
                .ok_or_else(|| anyhow!("{} is not a repository", dir.display()))
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
