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
//! may have replaced), and caller options that would turn any of that back on are refused.
//!
//! No setting keeps git out of a nested repository: `git add` (and `describe --dirty`) start
//! `git status` inside any gitlink whose directory holds a `.git`, whatever
//! `diff.ignoreSubmodules` or `submodule.<name>.ignore` say, and that runs the nested
//! repository's own filters. A job can make one itself (`git init` a directory, let one command
//! stage it as a gitlink, then edit it). So every command for a job tree is refused while any
//! directory below the work tree's root holds a `.git` entry (a file, directory or symlink of
//! that name, in any ASCII case): host git never opens a nested repository of a job tree. The
//! check walks the whole work tree before each command, without following symlinks (git does not
//! follow them to a nested repository either), and fails closed when a directory cannot be read
//! or the tree has more than [`NESTED_WALK_LIMIT`] entries. It sees the tree as it is at that
//! moment: a job still running could add a `.git` after it, which the submodule settings above
//! then only partly cover, so a command whose correctness matters runs once the job is gone.
//!
//! The git binary is resolved to an absolute path first, so the work tree is never searched for
//! it, and is refused when it lies inside a job root. The command gets a `PATH` of the absolute
//! entries outside every job root only, so the programs git itself looks up (`ssh`, a pager, a
//! `git-<name>` command, a remote helper) are never found in a job-written directory either.
//!
//! # Object directories
//!
//! Git also reads every object directory the repository's `objects/info/alternates` names (and
//! theirs in turn), and those a caller sets in `GIT_ALTERNATE_OBJECT_DIRECTORIES` or
//! `GIT_OBJECT_DIRECTORY`. Besides objects, an object directory holds files git parses as
//! trusted (a commit graph, a multi-pack index, bitmaps, reverse indexes), so one inside a job
//! root is refused like a job-written git directory: the alternates chain is read (absolute or
//! relative entries; a quoted entry, a chain deeper than git's own limit of
//! [`ALTERNATES_DEPTH`], or an entry that cannot be resolved is refused, since it cannot be
//! checked).
//!
//! The one exception is [`SafeGit::job_object_directory`], which importing a confined job's
//! commits needs: it names exactly one object directory inside a job root that git may read,
//! reached through the git directory's alternates or `GIT_ALTERNATE_OBJECT_DIRECTORIES`, never
//! as a git directory, common directory or `GIT_OBJECT_DIRECTORY`. It must be an absolute path
//! ending in `objects` that is a directory, not a symlink; the command must have no work tree;
//! its own alternates may not lead into any job root; and every command built with it gets
//! [`UNTRUSTED_OBJECTS_CONFIG`], so git reads only its loose objects and packs (whose contents
//! are checked by hash), never a commit graph, multi-pack index, bitmap or reverse index from
//! it. The caller checks what else lies in it (see [`crate::private_git`]).

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

/// The message of a refusal for a job tree holding a nested repository, followed by the
/// directory holding its `.git`.
const NESTED_REFUSAL: &str = "refusing to run git in a job tree that holds a nested repository";

/// The message of a refusal for an object directory inside a job root, followed by it.
const OBJECTS_REFUSAL: &str = "refusing to let git read a job-written object directory";

/// The most directory entries the nested repository check reads in one work tree; a larger tree
/// is refused rather than half checked.
pub const NESTED_WALK_LIMIT: usize = 1_000_000;

/// The largest work tree `.gitmodules` read for its submodule names; a larger one is refused.
const GITMODULES_LIMIT: u64 = 1 << 20;

/// The largest `objects/info/alternates` file read; a larger one is refused.
const ALTERNATES_LIMIT: u64 = 64 << 10;

/// The deepest alternates chain git follows (its own limit); a deeper one is refused.
pub const ALTERNATES_DEPTH: usize = 5;

/// The configuration a command reading untrusted object directories gets: git reads only loose
/// objects and packs, never the derived files (commit graph, multi-pack index, bitmaps, reverse
/// indexes) a job could have planted, whose contents git does not check against the objects.
pub const UNTRUSTED_OBJECTS_CONFIG: &[(&str, &str)] = &[
    ("core.commitGraph", "false"),
    ("core.multiPackIndex", "false"),
    ("pack.useBitmaps", "false"),
    ("pack.readReverseIndex", "false"),
];

/// The git binary a command runs and the `PATH` it gets.
struct Program {
    binary: PathBuf,
    path: OsString,
}

/// Builds host git commands for one repository and work tree, both named by the caller. See the
/// module documentation.
#[derive(Clone, Debug)]
pub struct SafeGit {
    binary: PathBuf,
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    config: Vec<(String, String)>,
    envs: Vec<(OsString, OsString)>,
    job_objects: Option<PathBuf>,
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
            job_objects: None,
        }
    }

    /// This git (binary, environment and configuration) for another git directory and work
    /// tree. A [`SafeGit::job_object_directory`] is not carried over.
    pub fn for_git_dir(&self, git_dir: impl Into<PathBuf>, work_tree: Option<PathBuf>) -> Self {
        Self {
            binary: self.binary.clone(),
            git_dir: git_dir.into(),
            work_tree,
            config: self.config.clone(),
            envs: self.envs.clone(),
            job_objects: None,
        }
    }

    /// The git directory the commands run on.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Lets git read the object directory `objects` although it lies inside a job root, as an
    /// alternate object directory only (see the module documentation). Every command then gets
    /// [`UNTRUSTED_OBJECTS_CONFIG`].
    pub fn job_object_directory(mut self, objects: impl Into<PathBuf>) -> Self {
        self.job_objects = Some(objects.into());
        self
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
        self.check_object_directories(in_job_root)?;

        let job_tree = self.work_tree.as_deref().filter(|tree| in_job_root(tree));
        if job_tree.is_some() {
            check_job_tree_args(&args[1..])?;
        }
        let program = self.program(in_job_root)?;
        if let Some(tree) = job_tree
            && let Some(nested) = find_nested_repository(tree)?
        {
            bail!("{NESTED_REFUSAL}: {}", nested.display());
        }
        let job_tree = job_tree.is_some();
        let overrides = if job_tree {
            self.job_tree_overrides(&program).await?
        } else {
            Vec::new()
        };

        let mut command = self.base_command(&program);
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

    /// The git binary as an absolute path, and the `PATH` the command gets: the entries of the
    /// caller's `PATH` (or the inherited one) that are absolute, existing directories outside
    /// every job root. The command runs in the work tree, where a relative entry would find a
    /// job-written program, and git looks up more than itself there. A bare binary name is looked
    /// up in that `PATH`; a binary inside a job root is refused.
    fn program(&self, in_job_root: &dyn Fn(&Path) -> bool) -> Result<Program> {
        let inherited = self
            .envs
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let entries: Vec<PathBuf> = std::env::split_paths(&inherited)
            .filter(|dir| dir.is_absolute() && dir.is_dir() && !in_job_root(dir))
            .collect();
        let path = std::env::join_paths(&entries).context("joining the PATH for git")?;

        let binary = if self.binary.is_absolute() {
            self.binary.clone()
        } else if self.binary.components().count() == 1 {
            entries
                .iter()
                .map(|dir| dir.join(&self.binary))
                .find(|candidate| is_executable_file(candidate))
                .ok_or_else(|| {
                    anyhow!(
                        "cannot find {} in an absolute PATH entry",
                        self.binary.display()
                    )
                })?
        } else {
            bail!(
                "the git binary must be an absolute path or a bare name, not {}",
                self.binary.display()
            );
        };
        if in_job_root(&binary) {
            bail!(
                "refusing to run a git binary inside a job root: {}",
                binary.display()
            );
        }
        Ok(Program { binary, path })
    }

    /// A git command for this repository with the inherited redirecting variables removed, the
    /// caller's environment and configuration set, then the protections every command gets (the
    /// cleaned `PATH`, no fsmonitor, no untracked cache, no implicit bare repository), and no
    /// subcommand yet.
    fn base_command(&self, program: &Program) -> util::command::Command {
        let mut command = util::command::new_command(&program.binary);
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
        command.env("PATH", &program.path);
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
        if self.job_objects.is_some() {
            for (key, value) in UNTRUSTED_OBJECTS_CONFIG {
                command.arg("-c").arg(format!("{key}={value}"));
            }
        }
        command
    }

    /// Refuses every object directory git would read that lies inside a job root, but the one
    /// [`SafeGit::job_object_directory`] names, reached as an alternate (see the module
    /// documentation).
    fn check_object_directories(&self, in_job_root: &dyn Fn(&Path) -> bool) -> Result<()> {
        if let Some(objects) = &self.job_objects {
            if self.work_tree.is_some() {
                bail!(
                    "{OBJECTS_REFUSAL}: {} (a job object directory is read without a work tree only)",
                    objects.display()
                );
            }
            let is_real_dir = fs::symlink_metadata(objects)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
            if !objects.is_absolute()
                || objects.file_name() != Some(OsStr::new("objects"))
                || !is_real_dir
            {
                bail!(
                    "{OBJECTS_REFUSAL}: {} (not an absolute objects directory)",
                    objects.display()
                );
            }
        }
        // Each object directory git would read, with the depth of the alternates chain that
        // reached it.
        let mut pending: Vec<(PathBuf, usize)> = Vec::new();
        for (key, value) in &self.envs {
            if key == "GIT_OBJECT_DIRECTORY" {
                let objects = Path::new(value);
                if in_job_root(objects) {
                    bail!("{OBJECTS_REFUSAL}: {}", objects.display());
                }
                pending.push((objects.to_path_buf(), 0));
            } else if key == "GIT_ALTERNATE_OBJECT_DIRECTORIES" {
                for entry in std::env::split_paths(value) {
                    if entry.as_os_str().is_empty() {
                        continue;
                    }
                    let quoted = entry.as_os_str().as_encoded_bytes().first() == Some(&b'"');
                    if quoted || !entry.is_absolute() {
                        bail!(
                            "{OBJECTS_REFUSAL}: cannot check the alternate {}",
                            entry.display()
                        );
                    }
                    self.check_alternate(&entry, false, in_job_root)?;
                    pending.push((entry, 1));
                }
            }
        }
        pending.push((common_dir_of(&self.git_dir)?.join("objects"), 0));

        let mut seen = Vec::new();
        while let Some((objects, depth)) = pending.pop() {
            if seen.contains(&objects) {
                continue;
            }
            let from_job = self.job_objects.as_deref() == Some(objects.as_path());
            let alternates = objects.join("info/alternates");
            let contents = read_regular_file_limited(&alternates, ALTERNATES_LIMIT)?;
            for line in contents
                .as_deref()
                .unwrap_or_default()
                .split(|byte| *byte == b'\n')
            {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() || line.first() == Some(&b'#') {
                    continue;
                }
                if depth >= ALTERNATES_DEPTH || line.first() == Some(&b'"') {
                    bail!(
                        "{OBJECTS_REFUSAL}: cannot check the alternates of {}",
                        objects.display()
                    );
                }
                let entry = objects.join(path_from_bytes(line)?);
                self.check_alternate(&entry, from_job, in_job_root)?;
                pending.push((entry, depth + 1));
            }
            seen.push(objects);
        }
        Ok(())
    }

    /// Refuses the alternate object directory `entry` when it lies inside a job root, unless it
    /// is the one [`SafeGit::job_object_directory`] names and was not reached through that one's
    /// own alternates (`from_job`).
    fn check_alternate(
        &self,
        entry: &Path,
        from_job: bool,
        in_job_root: &dyn Fn(&Path) -> bool,
    ) -> Result<()> {
        if !from_job && self.job_objects.as_deref() == Some(entry) {
            return Ok(());
        }
        if in_job_root(entry) {
            bail!("{OBJECTS_REFUSAL}: {}", entry.display());
        }
        Ok(())
    }

    /// The `-c` settings that switch off every driver and configured hook the configuration
    /// defines: each filter driver (`clean`, `smudge` and `process` emptied, `required` false),
    /// each diff driver's `command` and `textconv` (emptied: git then fails to run it instead of
    /// running anything), each merge driver's `driver` (emptied: the merge reports a conflict)
    /// and each configured hook (`hook.<name>.enabled` false). Plus `submodule.<name>.ignore=all`
    /// for every submodule a `.gitmodules` names (the work tree's, the index's and `HEAD`'s, as git
    /// reads them), since a job-written `submodule.<name>.ignore` would otherwise send a command
    /// that takes no `--ignore-submodules` (`stash`, for one) into the submodule's repository.
    /// (The nested repository refusal already keeps git out of every submodule; these settings
    /// remain for a `.git` a running job adds after that check.) The work tree's `.gitmodules` is
    /// read through a handle opened without following a symlink or blocking on a FIFO, and git
    /// reads those bytes from its standard input; one that is not a regular file (a FIFO git
    /// would block on forever, for one) refuses the command.
    ///
    /// A driver runs only when the configuration names its command, so listing the configuration
    /// (`git config --get-regexp`, run like the command itself: the same git directory, work tree,
    /// environment and caller configuration, so `include.path` and `includeIf` resolve the same
    /// way) finds every driver a `.gitattributes` could select. Reading the configuration runs
    /// nothing. A listing that fails, or a name that cannot be overridden through `-c`, fails the
    /// command instead of running it with a driver left on.
    async fn job_tree_overrides(&self, program: &Program) -> Result<Vec<String>> {
        let mut settings = Vec::new();
        let mut push = |setting: String| {
            if !settings.contains(&setting) {
                settings.push(setting);
            }
        };
        let drivers = self
            .config_keys(program, &[], None, r"^(filter|diff|merge|hook)\.")
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

        let work_tree_gitmodules = match &self.work_tree {
            Some(work_tree) => read_regular_file(&work_tree.join(".gitmodules"))?,
            None => None,
        };
        let mut sources = Vec::new();
        if let Some(contents) = &work_tree_gitmodules {
            sources.push(("--file=-", Some(contents.as_slice())));
        }
        sources.push(("--blob=:.gitmodules", None));
        sources.push(("--blob=HEAD:.gitmodules", None));
        for (source, stdin) in sources {
            let source = [OsString::from(source), OsString::from("--no-includes")];
            for (_, name, _) in self
                .config_keys(program, &source, stdin, r"^submodule\.")
                .await?
            {
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
        program: &Program,
        source: &[OsString],
        stdin: Option<&[u8]>,
        regexp: &str,
    ) -> Result<Vec<(String, String, String)>> {
        use futures::AsyncWriteExt as _;

        let mut command = self.base_command(program);
        // Reading a blob from the index reads the index, so no optional lock is taken.
        command
            .args(["--no-optional-locks", "config"])
            .args(source)
            .args(["--null", "--get-regexp", regexp]);
        let context = || format!("listing the git configuration {regexp}");
        let output = match stdin {
            None => command.output().await.with_context(context)?,
            Some(input) => {
                command
                    .stdin(util::command::Stdio::piped())
                    .stdout(util::command::Stdio::piped())
                    .stderr(util::command::Stdio::piped());
                let mut child = command.spawn().with_context(context)?;
                let mut pipe = child.stdin.take();
                // Written while the output is read, so neither pipe can stall the other.
                let feed = async move {
                    if let Some(pipe) = pipe.as_mut() {
                        pipe.write_all(input).await?;
                        pipe.close().await?;
                    }
                    drop(pipe);
                    Ok::<(), io::Error>(())
                };
                let (fed, output) = futures::join!(feed, child.output());
                let output = output.with_context(context)?;
                fed.with_context(context)?;
                output
            }
        };
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

/// [`read_regular_file_limited`] with [`GITMODULES_LIMIT`].
fn read_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
    read_regular_file_limited(path, GITMODULES_LIMIT)
}

/// The contents of `path` when it is a regular file, `None` when nothing is there. Opened without
/// following a symlink and without blocking (a FIFO would otherwise block the open), then checked
/// through the open handle, so what is read is what was checked. Anything else there, or a file
/// over `limit` bytes, is an error.
pub(crate) fn read_regular_file_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let not_regular = || anyhow!("refusing a {} that is not a regular file", path.display());
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        // `O_NOFOLLOW` fails a symlink with `ELOOP`.
        Err(_) if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) => {
            return Err(not_regular());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("opening {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    if metadata.len() > limit {
        bail!(
            "refusing {}: it is larger than {limit} bytes",
            path.display()
        );
    }
    let mut contents = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("reading {}", path.display()))?;
    if contents.len() as u64 > limit {
        bail!(
            "refusing {}: it is larger than {limit} bytes",
            path.display()
        );
    }
    Ok(Some(contents))
}

/// The first directory below `tree` (not `tree` itself) that holds an entry named `.git` in any
/// ASCII case, found by walking `tree` without following symlinks and without entering `tree`'s
/// own `.git`. Fails when a directory cannot be read or the walk passes [`NESTED_WALK_LIMIT`]
/// entries, so a tree is never taken as clean without having been read in full.
pub(crate) fn find_nested_repository(tree: &Path) -> Result<Option<PathBuf>> {
    let mut pending = vec![tree.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = pending.pop() {
        let reading = || format!("reading {} to look for a nested repository", dir.display());
        for entry in fs::read_dir(&dir).with_context(reading)? {
            let entry = entry.with_context(reading)?;
            seen += 1;
            if seen > NESTED_WALK_LIMIT {
                bail!(
                    "refusing to run git in a job tree of more than {NESTED_WALK_LIMIT} entries: {}",
                    tree.display()
                );
            }
            let is_git = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".git"));
            if is_git {
                if dir == tree {
                    continue;
                }
                return Ok(Some(dir));
            }
            // The type of the entry itself; a symlink is not followed.
            if entry.file_type().with_context(reading)?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(None)
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

/// The common directory of `git_dir`: the one its `commondir` file names, or `git_dir` itself.
fn common_dir_of(git_dir: &Path) -> Result<PathBuf> {
    let commondir_file = git_dir.join("commondir");
    match fs::read_to_string(&commondir_file) {
        Ok(text) => Ok(git_dir.join(text.trim())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(git_dir.to_path_buf()),
        Err(error) => Err(error).with_context(|| format!("reading {}", commondir_file.display())),
    }
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(bytes).context("an alternate path that is not UTF-8")?,
        ))
    }
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
