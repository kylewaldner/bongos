//! Host git for job trees: every git command bongos itself runs in a tree a job worked in, or to
//! import a job's work, is built here.
//!
//! A job may write its tree, so anything git would read from the tree or find from it is job
//! input: a planted `.git` (with its `config`, hooks and `core.fsmonitor`), a `.gitattributes`
//! that selects a configured text conversion, external diff or filter driver. [`SafeGit`] never
//! lets git discover its repository: the git directory and work tree come from the caller's own
//! record of the tree (`--git-dir`, `--work-tree`). It refuses a git directory, or the common
//! directory its `commondir` names, that lies inside a registered job root
//! ([`util::job_roots`]), since a job may have written its configuration. When the work tree
//! itself lies inside a job root, every driver a job-written `.gitattributes` could select and
//! every hook is switched off, git does not recurse into submodules (whose repositories the job
//! may have replaced), and caller options that would turn any of that back on are refused. The
//! git binary is resolved to an absolute path first, so the work tree is never searched for it.

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};

/// Commands that only read the repository; they run with `--no-optional-locks`, so they never
/// rewrite the index behind another git process.
const READ_ONLY_COMMANDS: &[&str] = &[
    "blame",
    "cat-file",
    "check-attr",
    "check-ignore",
    "describe",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "for-each-ref",
    "grep",
    "log",
    "ls-files",
    "ls-tree",
    "merge-base",
    "name-rev",
    "rev-list",
    "rev-parse",
    "shortlog",
    "show",
    "show-ref",
    "status",
];

/// The environment variables that would point git at another repository, index, object store or
/// configuration than the one given, or read objects and pathspecs differently. Removed from every
/// command's inherited environment.
const REDIRECTING_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_ATTR_SOURCE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// The message of a refusal, followed by the refused path.
const REFUSAL: &str = "refusing to run git with a job-written git directory";

/// Builds host git commands for one repository and work tree, both named by the caller. See the
/// module documentation.
#[derive(Clone, Debug)]
pub struct SafeGit {
    binary: PathBuf,
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    config: Vec<(String, String)>,
    envs: Vec<(OsString, OsString)>,
}

impl SafeGit {
    /// Git on the `PATH`, for the git directory `git_dir` with the work tree `work_tree`. Both come
    /// from the caller's record of the tree, never from a `.git` file in it (see
    /// [`linked_git_dir_from_record`]).
    pub fn new(git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Self {
        Self {
            work_tree: Some(work_tree.into()),
            ..Self::without_work_tree(git_dir)
        }
    }

    /// Git on the `PATH`, for the git directory `git_dir` and no work tree (an import into the
    /// host repository, for one).
    pub fn without_work_tree(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from("git"),
            git_dir: git_dir.into(),
            work_tree: None,
            config: Vec::new(),
            envs: Vec::new(),
        }
    }

    /// Runs the git binary at `binary` instead of the one on the `PATH`.
    pub fn binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Sets the configuration `key` to `value` for every command (`user.name`, for one). The
    /// protections this type adds are given after it, so they win.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.push((key.into(), value.into()));
        self
    }

    /// Sets an environment variable for every command, the filter listing included.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// The git command running `args`, which start with the subcommand (global options go through
    /// [`SafeGit::config`]). Refuses when the git directory or its common directory lies inside
    /// a registered job root.
    pub async fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<util::command::Command> {
        self.command_with(args, &util::job_roots::contains).await
    }

    /// [`SafeGit::command`] with `in_job_root` deciding what lies inside a job root.
    pub(crate) async fn command_with<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        in_job_root: &dyn Fn(&Path) -> bool,
    ) -> Result<util::command::Command> {
        let Some(subcommand) = args.first().map(AsRef::as_ref) else {
            bail!("a git command needs a subcommand");
        };
        if subcommand.to_string_lossy().starts_with('-') {
            bail!(
                "a git command starts with its subcommand, not the option {}",
                subcommand.to_string_lossy()
            );
        }
        check_git_dir_with(&self.git_dir, in_job_root)?;

        let job_tree = self
            .work_tree
            .as_deref()
            .is_some_and(|tree| in_job_root(tree));
        if job_tree {
            check_job_tree_args(&args[1..])?;
        }
        let binary = self.resolved_binary()?;
        let overrides = if job_tree {
            self.job_tree_overrides(&binary).await?
        } else {
            Vec::new()
        };

        let mut command = self.base_command(&binary);
        let subcommand_text = subcommand.to_string_lossy();
        if READ_ONLY_COMMANDS.contains(&subcommand_text.as_ref()) {
            command.arg("--no-optional-locks");
        }
        if job_tree {
            // The editor's overrides for an untrusted repository; no hook at all, since a
            // relative `core.hooksPath` resolves in the job-written tree and a host hook may run
            // tree content (`/dev/null` is not a directory, so git finds no hook in it); no
            // recursion into a submodule, whose repository the job may have replaced; and every
            // driver the configuration defines, since a job-written `.gitattributes` may select
            // any of them. These `-c` values also reach any git process git itself starts.
            command.args(["-c", "diff.external="]);
            command.args(["-c", "core.hooksPath=/dev/null"]);
            command.args(["-c", "diff.ignoreSubmodules=all"]);
            command.args(["-c", "submodule.recurse=false"]);
            command.args(["-c", "status.submoduleSummary=false"]);
            for setting in &overrides {
                command.arg("-c").arg(setting);
            }
        }
        command.arg(subcommand);
        if job_tree {
            match subcommand_text.as_ref() {
                "diff" | "log" | "show" => {
                    command.args(["--no-ext-diff", "--no-textconv"]);
                }
                "blame" => {
                    command.arg("--no-textconv");
                }
                _ => {}
            }
            // A job-written `.gitmodules` (`submodule.<name>.ignore = none`) overrides
            // `diff.ignoreSubmodules`; only the command-line option overrides it.
            if matches!(
                subcommand_text.as_ref(),
                "status" | "diff" | "diff-files" | "diff-index"
            ) {
                command.arg("--ignore-submodules=all");
            }
        }
        command.args(&args[1..]);
        Ok(command)
    }

    /// The git binary as an absolute path. A bare name is looked up in the `PATH` the command
    /// gets, skipping empty and relative entries: the command runs in the work tree, where a
    /// relative entry would find a job-written `git`.
    fn resolved_binary(&self) -> Result<PathBuf> {
        if self.binary.is_absolute() {
            return Ok(self.binary.clone());
        }
        if self.binary.components().count() != 1 {
            bail!(
                "the git binary must be an absolute path or a bare name, not {}",
                self.binary.display()
            );
        }
        let path = self
            .envs
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(&self.binary))
            .find(|candidate| is_executable_file(candidate))
            .ok_or_else(|| {
                anyhow!(
                    "cannot find {} in an absolute PATH entry",
                    self.binary.display()
                )
            })
    }

    /// A git command for this repository with the inherited redirecting variables removed, the
    /// caller's environment and configuration set, then the protections every command gets (no
    /// fsmonitor, no untracked cache, no implicit bare repository), and no subcommand yet.
    fn base_command(&self, binary: &Path) -> util::command::Command {
        let mut command = util::command::new_command(binary);
        for name in REDIRECTING_VARIABLES {
            command.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            let numbered = name.to_str().is_some_and(|name| {
                name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_")
            });
            if numbered {
                command.env_remove(name);
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        let mut git_dir = OsString::from("--git-dir=");
        git_dir.push(&self.git_dir);
        command.arg(git_dir);
        if let Some(work_tree) = &self.work_tree {
            let mut argument = OsString::from("--work-tree=");
            argument.push(work_tree);
            command.arg(argument).current_dir(work_tree);
        }
        for (key, value) in &self.config {
            command.arg("-c").arg(format!("{key}={value}"));
        }
        command.args(["-c", "core.fsmonitor=false"]);
        command.args(["-c", "safe.bareRepository=explicit"]);
        command.args(["-c", "core.untrackedCache=false"]);
        command
    }

    /// The `-c` settings that switch off every driver and configured hook the configuration
    /// defines: each filter driver (`clean`, `smudge` and `process` emptied, `required` false),
    /// each diff driver's `command` and `textconv` (emptied: git then fails to run it instead of
    /// running anything), each merge driver's `driver` (emptied: the merge reports a conflict)
    /// and each configured hook (`hook.<name>.enabled` false). Plus `submodule.<name>.ignore=all`
    /// for every submodule a `.gitmodules` names (the work tree's, the index's and `HEAD`'s, as git
    /// reads them), since a job-written `submodule.<name>.ignore` would otherwise send a command
    /// that takes no `--ignore-submodules` (`stash`, for one) into the submodule's repository.
    ///
    /// A driver runs only when the configuration names its command, so listing the configuration
    /// (`git config --get-regexp`, run like the command itself: the same git directory, work tree,
    /// environment and caller configuration, so `include.path` and `includeIf` resolve the same
    /// way) finds every driver a `.gitattributes` could select. Reading the configuration runs
    /// nothing. A listing that fails, or a name that cannot be overridden through `-c`, fails the
    /// command instead of running it with a driver left on.
    async fn job_tree_overrides(&self, binary: &Path) -> Result<Vec<String>> {
        let mut settings = Vec::new();
        let mut push = |setting: String| {
            if !settings.contains(&setting) {
                settings.push(setting);
            }
        };
        let drivers = self
            .config_keys(binary, &[], r"^(filter|diff|merge|hook)\.")
            .await?;
        for (section, name, variable) in &drivers {
            match (section.as_str(), variable.as_str()) {
                ("filter", _) => {
                    for variable in ["clean", "smudge", "process"] {
                        push(format!("filter.{name}.{variable}="));
                    }
                    push(format!("filter.{name}.required=false"));
                }
                ("diff", "command" | "textconv") | ("merge", "driver") => {
                    push(format!("{section}.{name}.{variable}="));
                }
                ("hook", "command") => push(format!("hook.{name}.enabled=false")),
                _ => {}
            }
        }

        let mut sources = Vec::new();
        if let Some(work_tree) = &self.work_tree {
            let gitmodules = work_tree.join(".gitmodules");
            if fs::symlink_metadata(&gitmodules).is_ok() {
                let mut file = OsString::from("--file=");
                file.push(&gitmodules);
                sources.push(file);
            }
        }
        sources.push(OsString::from("--blob=:.gitmodules"));
        sources.push(OsString::from("--blob=HEAD:.gitmodules"));
        for source in sources {
            let source = [source, OsString::from("--no-includes")];
            for (_, name, _) in self.config_keys(binary, &source, r"^submodule\.").await? {
                push(format!("submodule.{name}.ignore=all"));
            }
        }
        Ok(settings)
    }

    /// Every `section.name.variable` key matching `regexp` in the configuration `source` (the
    /// repository's when empty), as its section, name and variable; keys without a name are
    /// skipped. A missing source counts as empty (`git config` exits with 1 for it, as when
    /// nothing matches). Refuses a name holding '=', which `-c` cannot override.
    async fn config_keys(
        &self,
        binary: &Path,
        source: &[OsString],
        regexp: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let mut command = self.base_command(binary);
        // Reading a blob from the index reads the index, so no optional lock is taken.
        command
            .args(["--no-optional-locks", "config"])
            .args(source)
            .args(["--null", "--get-regexp", regexp]);
        let output = command
            .output()
            .await
            .with_context(|| format!("listing the git configuration {regexp}"))?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "listing the git configuration {regexp} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut keys = Vec::new();
        for record in output.stdout.split(|byte| *byte == 0) {
            let key = record.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
            let key = String::from_utf8_lossy(key);
            // `section.name.variable`; the name (a subsection) may itself hold dots.
            let Some((section, rest)) = key.split_once('.') else {
                continue;
            };
            let Some((name, variable)) = rest.rsplit_once('.') else {
                continue;
            };
            if name.contains('=') {
                bail!("cannot switch off the git {section} {name:?}: its name holds '='");
            }
            keys.push((section.to_string(), name.to_string(), variable.to_string()));
        }
        Ok(keys)
    }
}

/// Options a caller may not pass for a job tree, each with the shortest prefix git would accept
/// as an abbreviation of it: they turn back on what the protections switch off.
const REFUSED_OPTIONS: &[(&str, usize)] = &[
    ("ext-diff", 2),
    ("textconv", 5),
    ("recurse-submodules", 2),
    ("ignore-submodules", 2),
];

/// Refuses caller arguments that would undo a job tree's protections: `--ext-diff`,
/// `--textconv`, `--recurse-submodules` and `--ignore-submodules` (any value), in full or
/// abbreviated. Global options such as `-c` cannot be passed at all, since the arguments start
/// with the subcommand.
fn check_job_tree_args<S: AsRef<OsStr>>(args: &[S]) -> Result<()> {
    for arg in args {
        let arg = arg.as_ref().to_string_lossy();
        if arg == "--" {
            break;
        }
        let Some(name) = arg.strip_prefix("--") else {
            continue;
        };
        let name = name.split_once('=').map_or(name, |(name, _)| name);
        let refused = REFUSED_OPTIONS
            .iter()
            .any(|(option, shortest)| name.len() >= *shortest && option.starts_with(name));
        if refused {
            bail!("refusing the git option {arg} in a job tree");
        }
    }
    Ok(())
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Refuses `git_dir` when it, its `commondir` target or the common directory lies inside a
/// registered job root. A path that cannot be resolved counts as inside, as in
/// [`util::job_roots::contains`].
pub fn check_git_dir(git_dir: &Path) -> Result<()> {
    check_git_dir_with(git_dir, &util::job_roots::contains)
}

/// [`check_git_dir`] with `in_job_root` deciding what lies inside a job root.
pub fn check_git_dir_with(git_dir: &Path, in_job_root: &dyn Fn(&Path) -> bool) -> Result<()> {
    let refuse = |path: &Path| anyhow!("{REFUSAL}: {}", path.display());
    if in_job_root(git_dir) {
        return Err(refuse(git_dir));
    }
    // Read through the kernel's resolution of `git_dir`, as git reads it.
    let commondir_file = git_dir.join("commondir");
    let text = match fs::read_to_string(&commondir_file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(refuse(&commondir_file)),
    };
    let target = Path::new(text.trim());
    // `join` keeps an absolute target as it is.
    let common_dir = git_dir.join(target);
    // `contains` canonicalizes, so this is the common directory git would use.
    if in_job_root(&common_dir) {
        return Err(refuse(&common_dir));
    }
    Ok(())
}

/// The git directory of the linked worktree `tree`, read from its `.git` file and trusted only
/// where git puts it: a directory directly under `common_dir/worktrees/` that holds `gitdir` and
/// `commondir` files and whose `gitdir` file names this tree's `.git`. Anything else (a `.git`
/// directory or symlink, a pointer elsewhere or to another worktree's git directory, a symlinked
/// git directory) is refused. Returns the canonical git directory.
pub fn linked_git_dir_from_record(tree: &Path, common_dir: &Path) -> Result<PathBuf> {
    let refuse = |reason: &str| {
        anyhow!(
            "the .git of {} does not name a linked worktree of {}: {reason}",
            tree.display(),
            common_dir.display()
        )
    };
    let gitlink = tree.join(".git");
    let is_file = |path: &Path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
    };
    if !is_file(&gitlink) {
        return Err(refuse("it is not a regular file"));
    }
    let text = fs::read_to_string(&gitlink).map_err(|error| refuse(&error.to_string()))?;
    let target = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| refuse("it has no gitdir line"))?;
    let git_dir = tree.join(target);
    let is_dir = fs::symlink_metadata(&git_dir).is_ok_and(|metadata| metadata.file_type().is_dir());
    if !is_dir {
        return Err(refuse("its git directory is not a directory"));
    }
    let parent = git_dir
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    let worktrees = fs::canonicalize(common_dir.join("worktrees")).ok();
    if parent.is_none() || parent != worktrees {
        return Err(refuse("its git directory is not directly under worktrees/"));
    }
    if !is_file(&git_dir.join("gitdir")) || !is_file(&git_dir.join("commondir")) {
        return Err(refuse("its git directory lacks a gitdir or commondir file"));
    }
    // The git directory must record this tree's `.git` as its own, or it belongs to another
    // worktree (git writes an absolute path there, or one relative to the git directory).
    let recorded =
        fs::read_to_string(git_dir.join("gitdir")).map_err(|error| refuse(&error.to_string()))?;
    let recorded = git_dir.join(recorded.trim_end_matches(['\n', '\r']));
    let same_tree = match (fs::canonicalize(&recorded), fs::canonicalize(&gitlink)) {
        (Ok(recorded), Ok(gitlink)) => recorded == gitlink,
        _ => false,
    };
    if !same_tree {
        return Err(refuse("its git directory records another worktree"));
    }
    fs::canonicalize(&git_dir).map_err(|error| refuse(&error.to_string()))
}
