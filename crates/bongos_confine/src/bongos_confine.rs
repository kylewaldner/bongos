//! Job confinement: what a job bongos runs (a workflow program, an agent step or a task check)
//! may write.
//!
//! A job declares its trees, its scratch directory and the directories it needs. From that
//! declaration [`Confinement::build`] makes one allowlist of writable roots, the read-only masks
//! inside them (each tree's `.git`, and for a main worktree its nested worktrees and the `.git` of
//! each nested repository) and the [`SandboxPolicy`] that enforces them. Nothing else is writable.
//! Building records the writable roots in the job roots registry ([`util::job_roots`]), so no
//! git the editor or bongos runs ever trusts git data the job could have written;
//! [`Confinement::finish`] retires a main worktree's root once the job is gone.
//!
//! Every host git command bongos runs in a job tree, or to import a job's work, goes through
//! [`safe_git::SafeGit`], which takes the git directory from the caller's record of the tree and
//! refuses one a job could have written. A job in a linked worktree with [`TreeGit::Private`]
//! gets a git directory of its own in its scratch from [`private_git::prepare`].

pub mod annotate;
pub mod private_git;
mod promise_text;
pub mod safe_git;

#[cfg(test)]
mod private_git_tests;
#[cfg(test)]
mod safe_git_tests;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sandbox::{GitSandboxPolicy, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
use util::job_roots::RootLifetime;

pub use promise_text::{Platform, Promise, SandboxAvailability, promise, switch_off_notes};

/// Whether the platform's sandbox can run here, probed once per process. On Linux this runs a
/// brief `bwrap` probe, so call it off a hot path.
pub fn sandbox_availability() -> SandboxAvailability {
    static AVAILABILITY: std::sync::OnceLock<SandboxAvailability> = std::sync::OnceLock::new();
    *AVAILABILITY.get_or_init(|| {
        let policy = SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![std::env::temp_dir()],
            },
            network: SandboxNetPolicy::Unrestricted,
            git: GitSandboxPolicy::default(),
            read_only_masks: Vec::new(),
        };
        match sandbox::Sandbox::can_create(&policy, None) {
            Ok(()) => SandboxAvailability::Available,
            Err(_) => SandboxAvailability::Unavailable,
        }
    })
}

/// The rule a refused write broke, quoted in every refusal bongos reports.
pub const RULE: &str = "write outside the job tree and declared directories refused";

/// What kind of job is confined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobKind {
    Program,
    Agent,
    Check,
}

/// What git the job gets in a tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeGit {
    /// A private git directory in the job's scratch, imported after the pass (a linked worktree).
    Private,
    /// The tree's `.git` is masked read-only: git reads work, writes fail.
    ReadOnly,
    /// The tree has no `.git`.
    None,
}

/// Whether the job may write its tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeAccess {
    Writable,
    /// The tree stays under the read-only root and is not a writable root.
    ReadOnly,
}

/// One tree a job works in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobTree {
    pub path: PathBuf,
    pub access: TreeAccess,
    pub git: TreeGit,
}

/// Everything a job may write, as the job declared it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobDeclaration {
    pub kind: JobKind,
    /// Usually one.
    pub trees: Vec<JobTree>,
    /// The job's own scratch (TMPDIR, result file, private git).
    pub scratch: PathBuf,
    /// Directories the job declared it writes (lane and profile directories).
    pub declared_dirs: Vec<PathBuf>,
    /// Agent CLI state paths, for [`JobKind::Agent`] only. The runner drops entries that do not
    /// exist before building.
    pub extra_writable: Vec<PathBuf>,
    pub network: SandboxNetPolicy,
}

/// What the host knows and trusts about the job's repositories, read by bongos before the job
/// starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostGit {
    /// Git directories the host trusts for the job's trees: each tree's linked git directory, the
    /// common directory and the repository's other worktrees' git directories.
    pub git_dirs: Vec<PathBuf>,
    /// The run records directory, which no writable root may contain.
    pub records_dir: Option<PathBuf>,
}

/// Paths under a writable root that are not job-written, because they stayed masked read-only
/// for the whole job: a main worktree's own `.git` and any nested worktree that is a direct child
/// of the root. Deeper masks (the `.git` of a nested repository, a nested worktree further down)
/// are not listed: the job may rename an ancestor between the root and the mask and recreate the
/// path, so what is there after the job may be job-written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootExclusions {
    pub root: PathBuf,
    pub excluded: Vec<PathBuf>,
}

/// Why a job may not start.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfineError {
    #[error("a job path must be absolute: {}", path.display())]
    RelativePath { path: PathBuf },
    #[error("cannot resolve the job path {}: {error}", path.display())]
    Unresolved { path: PathBuf, error: String },
    #[error("a job may not write the whole filesystem: {} resolves to /", path.display())]
    FilesystemRoot { path: PathBuf },
    #[error(
        "the writable path {} overlaps the git directory {}",
        root.display(),
        git_dir.display()
    )]
    GitDirectory { root: PathBuf, git_dir: PathBuf },
    #[error(
        "the writable path {} overlaps the run records directory {}",
        root.display(),
        records_dir.display()
    )]
    RecordsDirectory { root: PathBuf, records_dir: PathBuf },
    #[error("a read-only tree cannot have private git: {}", tree.display())]
    ReadOnlyTreeWithPrivateGit { tree: PathBuf },
    #[error(
        "the read-only tree {} lies inside the writable path {}",
        tree.display(),
        root.display()
    )]
    ReadOnlyTreeInsideWritable { tree: PathBuf, root: PathBuf },
    #[error("a job in a main worktree gets read-only git, not private git: {}", tree.display())]
    MainWorktreeWithPrivateGit { tree: PathBuf },
    #[error("a read-only mask must not be a symlink: {}", path.display())]
    MaskSymlink { path: PathBuf },
    #[error("a read-only mask must lie inside a writable root: {}", path.display())]
    MaskOutsideRoots { path: PathBuf },
    #[error("cannot read {} to find nested repositories: {error}", path.display())]
    Walk { path: PathBuf, error: String },
    #[error("cannot record the job's writable roots: {error}")]
    Register { error: String },
    #[error("cannot retire the job root {}: {error}", root.display())]
    Retire { root: PathBuf, error: String },
}

/// Makes `<data dir>/confinement/roots.json` this process's job roots registry. Runs once per
/// process; later calls do nothing. Called at startup, after a confined helper process has been
/// dispatched, so a helper never opens the registry.
pub fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let path = paths::data_dir().join("confinement").join("roots.json");
        if let Err(error) = util::job_roots::init(&path) {
            log::error!(
                "confinement: cannot prune the job roots registry {}: {error:#}",
                path.display()
            );
        }
    });
}

/// Where a build records its roots: the process registry, or a given one in tests.
pub(crate) trait Registry {
    fn register(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        lifetime: RootLifetime,
    ) -> anyhow::Result<()>;
    fn retire(&self, root: &Path) -> anyhow::Result<()>;
}

/// The registry [`init`] installed.
struct ProcessRegistry;

impl Registry for ProcessRegistry {
    fn register(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        lifetime: RootLifetime,
    ) -> anyhow::Result<()> {
        util::job_roots::register(root, excluded, lifetime)
    }

    fn retire(&self, root: &Path) -> anyhow::Result<()> {
        util::job_roots::retire(root)
    }
}

#[cfg(test)]
impl Registry for util::job_roots::JobRoots {
    fn register(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        lifetime: RootLifetime,
    ) -> anyhow::Result<()> {
        util::job_roots::JobRoots::register(self, root, excluded, lifetime)
    }

    fn retire(&self, root: &Path) -> anyhow::Result<()> {
        util::job_roots::JobRoots::retire(self, root)
    }
}

/// The confinement of one job: its writable roots and the read-only masks inside them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Confinement {
    writable_roots: Vec<PathBuf>,
    masks: Vec<PathBuf>,
    exclusions: Vec<RootExclusions>,
    network: SandboxNetPolicy,
    /// The roots recorded in the job roots registry, each with its lifetime.
    registered: Vec<(PathBuf, RootLifetime)>,
    /// This build's place in [`HOLDERS`]; clones share it.
    holder: u64,
}

/// For each job-lifetime root of this process, the builds of this process still holding it.
/// Every job-lifetime entry this process records has the same owner (this process), so the
/// registry keeps one entry per root, and two jobs of this process in the same main worktree
/// share it: the entry is recorded by the first build to hold the root and retired only when the
/// last holder finishes. Locked across the registry calls, so a build and a finish on the same
/// root in this process never interleave.
///
/// The count is per process only. `roots.json` is shared by every bongos process of the user,
/// and the registry keeps one job-lifetime entry per root whatever its owner: a second process's
/// job in the same main worktree replaces the first process's entry (with its own exclusions and
/// owner), and either process's last finish retires every entry for the root, while the other
/// process's job may still be running. Two app processes running jobs in one main worktree at
/// once are not covered.
static HOLDERS: std::sync::Mutex<BTreeMap<PathBuf, BTreeSet<u64>>> =
    std::sync::Mutex::new(BTreeMap::new());

fn holders() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, BTreeSet<u64>>> {
    HOLDERS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Drops `holder` from `root`, and retires the root when no build holds it any more.
fn release(
    holders: &mut BTreeMap<PathBuf, BTreeSet<u64>>,
    root: &Path,
    holder: u64,
    registry: &dyn Registry,
) -> anyhow::Result<()> {
    if let Some(set) = holders.get_mut(root) {
        set.remove(&holder);
        if !set.is_empty() {
            return Ok(());
        }
        holders.remove(root);
    }
    registry.retire(root)
}

impl Confinement {
    /// Builds the confinement from a declaration, or refuses the job start.
    ///
    /// Every path is canonicalized first. The build refuses a relative or unresolvable path, a
    /// writable root that is `/`, a writable root that equals, contains or lies inside a trusted
    /// git directory (except a main worktree's own `.git`, which is masked), a writable root
    /// that contains or lies inside the run records directory, a read-only tree with private
    /// git, a read-only tree that equals or lies inside a writable root and a main worktree with
    /// private git. A read-only tree is not a writable root.
    ///
    /// Once the declaration is accepted, every writable root is recorded in the process's job
    /// roots registry: a main worktree (a writable tree whose `.git` is a directory) for the
    /// lifetime of this process, without its masked `.git` and the other masks directly under
    /// it (see [`RootExclusions`]); every other root (the trees and scratch bongos made, the
    /// declared directories) permanently. When a root cannot be recorded the build fails, after
    /// retiring the main worktrees it had recorded; the permanent entries stay, which refuses
    /// more than needed but never less. Several jobs of this process may work in the same main
    /// worktree at once: its entry stays until the last of them finishes (see
    /// [`Confinement::finish`]). Jobs of two processes in one main worktree are not tracked
    /// together (see [`HOLDERS`]).
    pub fn build(declaration: &JobDeclaration, host: &HostGit) -> Result<Self, ConfineError> {
        Self::build_in(declaration, host, &ProcessRegistry)
    }

    /// [`Confinement::build`], recording the roots in `registry`.
    pub(crate) fn build_in(
        declaration: &JobDeclaration,
        host: &HostGit,
        registry: &dyn Registry,
    ) -> Result<Self, ConfineError> {
        let mut confinement = Self::plan(declaration, host)?;
        confinement.register(registry)?;
        Ok(confinement)
    }

    /// Checks the declaration and works out the roots and masks, writing nothing.
    fn plan(declaration: &JobDeclaration, host: &HostGit) -> Result<Self, ConfineError> {
        let mut roots = vec![canonical(&declaration.scratch)?];
        let mut writable_trees = Vec::new();
        let mut read_only_trees = Vec::new();
        for tree in &declaration.trees {
            if tree.access == TreeAccess::ReadOnly && tree.git == TreeGit::Private {
                return Err(ConfineError::ReadOnlyTreeWithPrivateGit {
                    tree: tree.path.clone(),
                });
            }
            let path = canonical(&tree.path)?;
            match tree.access {
                TreeAccess::Writable => {
                    roots.push(path.clone());
                    writable_trees.push((path, tree.git));
                }
                TreeAccess::ReadOnly => read_only_trees.push(path),
            }
        }
        for dir in declaration
            .declared_dirs
            .iter()
            .chain(&declaration.extra_writable)
        {
            roots.push(canonical(dir)?);
        }
        dedup(&mut roots);

        if let Some(root) = roots.iter().find(|root| root.parent().is_none()) {
            return Err(ConfineError::FilesystemRoot { path: root.clone() });
        }

        for tree in &read_only_trees {
            if let Some(root) = roots.iter().find(|root| tree.starts_with(root)) {
                return Err(ConfineError::ReadOnlyTreeInsideWritable {
                    tree: tree.clone(),
                    root: root.clone(),
                });
            }
        }

        let mut main_worktrees = Vec::new();
        let mut linked_git_files = Vec::new();
        for (tree, git) in &writable_trees {
            let dot_git = tree.join(".git");
            match std::fs::symlink_metadata(&dot_git) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ConfineError::MaskSymlink { path: dot_git });
                }
                Ok(metadata) if metadata.is_dir() => {
                    if *git == TreeGit::Private {
                        return Err(ConfineError::MainWorktreeWithPrivateGit {
                            tree: tree.clone(),
                        });
                    }
                    main_worktrees.push(tree.clone());
                }
                Ok(_) => linked_git_files.push(dot_git),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(unresolved(&dot_git, error)),
            }
        }

        let mut trusted = Vec::new();
        for git_dir in &host.git_dirs {
            trusted.push(canonical(git_dir)?);
        }
        trusted.extend(main_worktrees.iter().map(|tree| tree.join(".git")));
        dedup(&mut trusted);
        for root in &roots {
            for git_dir in &trusted {
                let overlaps =
                    root == git_dir || git_dir.starts_with(root) || root.starts_with(git_dir);
                let masked_main_git = main_worktrees
                    .iter()
                    .any(|tree| tree == root && git_dir.starts_with(tree.join(".git")));
                if overlaps && !masked_main_git {
                    return Err(ConfineError::GitDirectory {
                        root: root.clone(),
                        git_dir: git_dir.clone(),
                    });
                }
            }
        }

        if let Some(records_dir) = &host.records_dir {
            let records_dir = canonical(records_dir)?;
            if let Some(root) = roots
                .iter()
                .find(|root| records_dir.starts_with(root) || root.starts_with(&records_dir))
            {
                return Err(ConfineError::RecordsDirectory {
                    root: root.clone(),
                    records_dir,
                });
            }
        }

        let mut masks = linked_git_files;
        let mut exclusions = Vec::new();
        for tree in &main_worktrees {
            let dot_git = tree.join(".git");
            let nested_worktrees = nested_worktrees(tree)?;
            let nested_repositories = nested_repositories(tree, &nested_worktrees)?;
            // Only a mask directly under the root is held in place by the root's own mount. A
            // deeper mask can be sidestepped: the job renames an ancestor between the root and
            // the mask (`mv vendor v2`) and recreates the path, so a nested repository's `.git`
            // or a deeper nested worktree may be job-written after the job and is not excluded.
            let excluded = std::iter::once(&dot_git)
                .chain(&nested_worktrees)
                .chain(&nested_repositories)
                .filter(|mask| mask.parent() == Some(tree.as_path()))
                .cloned()
                .collect();
            masks.push(dot_git);
            masks.extend(nested_worktrees);
            masks.extend(nested_repositories);
            exclusions.push(RootExclusions {
                root: tree.clone(),
                excluded,
            });
        }
        dedup(&mut masks);
        for mask in &masks {
            check_mask(mask, &roots)?;
        }

        Ok(Self {
            writable_roots: roots,
            masks,
            exclusions,
            network: declaration.network.clone(),
            registered: Vec::new(),
            holder: {
                static NEXT_HOLDER: AtomicU64 = AtomicU64::new(0);
                NEXT_HOLDER.fetch_add(1, Ordering::Relaxed)
            },
        })
    }

    /// Records every writable root in `registry` (see [`Confinement::build`]).
    fn register(&mut self, registry: &dyn Registry) -> Result<(), ConfineError> {
        let register_error = |error: String| ConfineError::Register { error };
        let mut holders = holders();
        let mut job_lifetime = None;
        for root in &self.writable_roots {
            let exclusions = self.exclusions.iter().find(|entry| &entry.root == root);
            let (excluded, lifetime) = match exclusions {
                Some(entry) => {
                    let lifetime = match job_lifetime {
                        Some(lifetime) => lifetime,
                        None => {
                            let lifetime = this_process_lifetime().map_err(register_error)?;
                            job_lifetime = Some(lifetime);
                            lifetime
                        }
                    };
                    (entry.excluded.as_slice(), lifetime)
                }
                None => (&[][..], RootLifetime::Permanent),
            };
            let result = if lifetime == RootLifetime::Permanent {
                registry.register(root, excluded, lifetime)
            } else {
                // A root another build of this process already holds keeps the entry that
                // build recorded: its exclusions were read before any job of this process could
                // write the tree, while this build's may name paths the running job made.
                let set = holders.entry(root.clone()).or_default();
                let first = set.is_empty();
                set.insert(self.holder);
                let result = if first {
                    registry.register(root, excluded, lifetime)
                } else {
                    Ok(())
                };
                if result.is_err()
                    && let Some(set) = holders.get_mut(root)
                {
                    set.remove(&self.holder);
                    if set.is_empty() {
                        holders.remove(root);
                    }
                }
                result
            };
            if let Err(error) = result {
                // Undo the job-lifetime entries; a permanent entry may predate this build, so
                // it stays.
                for (root, lifetime) in &self.registered {
                    if *lifetime != RootLifetime::Permanent
                        && let Err(retire_error) =
                            release(&mut holders, root, self.holder, registry)
                    {
                        log::error!(
                            "confinement: cannot retire {} after a failed build: {retire_error:#}",
                            root.display()
                        );
                    }
                }
                self.registered.clear();
                return Err(register_error(format!("{error:#}")));
            }
            self.registered.push((root.clone(), lifetime));
        }
        Ok(())
    }

    /// Retires the job-lifetime roots this build recorded (the main worktrees) that no other
    /// unfinished build of this process holds; permanent roots stay. Call it once the job's
    /// process group is gone, so nothing the job runs can still write the tree. A second call
    /// releases nothing more. Tries every root, and returns the first failure. A job of another
    /// process in the same main worktree is not counted: its entry is retired too (see
    /// [`HOLDERS`]).
    pub fn finish(&self) -> Result<(), ConfineError> {
        self.finish_in(&ProcessRegistry)
    }

    /// [`Confinement::finish`] on `registry`.
    pub(crate) fn finish_in(&self, registry: &dyn Registry) -> Result<(), ConfineError> {
        let mut holders = holders();
        let mut result = Ok(());
        for (root, lifetime) in &self.registered {
            if *lifetime == RootLifetime::Permanent {
                continue;
            }
            if let Err(error) = release(&mut holders, root, self.holder, registry)
                && result.is_ok()
            {
                result = Err(ConfineError::Retire {
                    root: root.clone(),
                    error: format!("{error:#}"),
                });
            }
        }
        result
    }

    /// The canonical directories the job may write.
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// The canonical paths inside the writable roots that stay read-only.
    pub fn masks(&self) -> &[PathBuf] {
        &self.masks
    }

    /// For each main worktree, the paths that stayed read-only and so are not job-written.
    pub fn exclusions(&self) -> &[RootExclusions] {
        &self.exclusions
    }

    /// Whether the job works in a main worktree, whose masks hold only under an enforcing
    /// sandbox, so the job may not run without one.
    pub fn requires_sandbox(&self) -> bool {
        !self.exclusions.is_empty()
    }

    /// The sandbox policy that enforces this confinement.
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: self.writable_roots.clone(),
            },
            network: self.network.clone(),
            git: GitSandboxPolicy::Denied {
                git_dirs: Vec::new(),
            },
            read_only_masks: self.masks.clone(),
        }
    }
}

/// The lifetime of an entry owned by this process: it is retired once this process is gone.
fn this_process_lifetime() -> Result<RootLifetime, String> {
    let pid = std::process::id();
    let start_time = util::job_roots::process_start_time(pid);
    // Only Linux reports start times; elsewhere an owner always counts as alive.
    if cfg!(target_os = "linux") && start_time.is_none() {
        return Err(format!(
            "cannot read the start time of this process ({pid})"
        ));
    }
    Ok(RootLifetime::Job {
        pid,
        start_time: start_time.unwrap_or_default(),
    })
}

fn unresolved(path: &Path, error: io::Error) -> ConfineError {
    ConfineError::Unresolved {
        path: path.to_path_buf(),
        error: error.to_string(),
    }
}

fn canonical(path: &Path) -> Result<PathBuf, ConfineError> {
    if !path.is_absolute() {
        return Err(ConfineError::RelativePath {
            path: path.to_path_buf(),
        });
    }
    std::fs::canonicalize(path).map_err(|error| unresolved(path, error))
}

fn dedup(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

fn check_mask(mask: &Path, roots: &[PathBuf]) -> Result<(), ConfineError> {
    let metadata = std::fs::symlink_metadata(mask).map_err(|error| unresolved(mask, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ConfineError::MaskSymlink {
            path: mask.to_path_buf(),
        });
    }
    if canonical(mask)? != mask {
        return Err(ConfineError::MaskSymlink {
            path: mask.to_path_buf(),
        });
    }
    if !roots
        .iter()
        .any(|root| mask != root && mask.starts_with(root))
    {
        return Err(ConfineError::MaskOutsideRoots {
            path: mask.to_path_buf(),
        });
    }
    Ok(())
}

/// The repository's other worktrees that lie inside a main worktree, read from the common
/// directory's `worktrees/*/gitdir`. An entry whose path no longer exists is skipped.
fn nested_worktrees(tree: &Path) -> Result<Vec<PathBuf>, ConfineError> {
    let entries_dir = tree.join(".git").join("worktrees");
    let entries = match std::fs::read_dir(&entries_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(walk_error(&entries_dir, error)),
    };
    let mut worktrees = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| walk_error(&entries_dir, error))?;
        let gitdir_file = entry.path().join("gitdir");
        let text = match std::fs::read_to_string(&gitdir_file) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(walk_error(&gitdir_file, error)),
        };
        let dot_git = entry.path().join(text.trim());
        match std::fs::symlink_metadata(&dot_git) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(unresolved(&dot_git, error)),
        }
        let Some(worktree) = dot_git.parent() else {
            continue;
        };
        let worktree = canonical(worktree)?;
        if worktree != tree && worktree.starts_with(tree) {
            worktrees.push(worktree);
        }
    }
    worktrees.sort();
    Ok(worktrees)
}

/// The `.git` of each repository nested in a main worktree, found by a walk that never follows
/// a symlink, never descends into a `.git` and skips the nested worktrees (masked whole).
fn nested_repositories(
    tree: &Path,
    nested_worktrees: &[PathBuf],
) -> Result<Vec<PathBuf>, ConfineError> {
    let own_git = tree.join(".git");
    let mut found = Vec::new();
    let mut pending = vec![tree.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| walk_error(&dir, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| walk_error(&dir, error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| walk_error(&path, error))?;
            if file_type.is_symlink() || path == own_git {
                continue;
            }
            if entry.file_name() == ".git" {
                found.push(path);
            } else if file_type.is_dir() && !nested_worktrees.contains(&path) {
                pending.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn walk_error(path: &Path, error: io::Error) -> ConfineError {
    ConfineError::Walk {
        path: path.to_path_buf(),
        error: error.to_string(),
    }
}
