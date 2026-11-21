//! Job confinement: what a job bongos runs (a workflow program, an agent step or a task check)
//! may write.
//!
//! A job declares its trees, its scratch directory and the directories it needs. From that
//! declaration [`Confinement::build`] makes one allowlist of writable roots, the read-only masks
//! inside them (each tree's `.git`, and for a main worktree its nested worktrees and the `.git` of
//! each nested repository) and the [`SandboxPolicy`] that enforces them. Nothing else is writable.
//! Building is pure: it reads the filesystem and writes nothing.

pub mod annotate;
mod promise_text;

#[cfg(test)]
mod tests;

use std::{
    io,
    path::{Path, PathBuf},
};

use sandbox::{GitSandboxPolicy, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};

pub use promise_text::{Platform, Promise, SandboxAvailability, promise, switch_off_notes};

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
/// for the whole job (a main worktree's `.git` and the `.git` of each nested repository).
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
        "the writable path {} contains the run records directory {}",
        root.display(),
        records_dir.display()
    )]
    RecordsDirectory { root: PathBuf, records_dir: PathBuf },
    #[error("a read-only tree cannot have private git: {}", tree.display())]
    ReadOnlyTreeWithPrivateGit { tree: PathBuf },
    #[error("a job in a main worktree gets read-only git, not private git: {}", tree.display())]
    MainWorktreeWithPrivateGit { tree: PathBuf },
    #[error("a read-only mask must not be a symlink: {}", path.display())]
    MaskSymlink { path: PathBuf },
    #[error("a read-only mask must lie inside a writable root: {}", path.display())]
    MaskOutsideRoots { path: PathBuf },
    #[error("cannot read {} to find nested repositories: {error}", path.display())]
    Walk { path: PathBuf, error: String },
}

/// The confinement of one job: its writable roots and the read-only masks inside them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Confinement {
    writable_roots: Vec<PathBuf>,
    masks: Vec<PathBuf>,
    exclusions: Vec<RootExclusions>,
    network: SandboxNetPolicy,
}

impl Confinement {
    /// Builds the confinement from a declaration, or refuses the job start.
    ///
    /// Every path is canonicalized first. The build refuses a relative or unresolvable path, a
    /// writable root that is `/`, a writable root that equals, contains or lies inside a trusted
    /// git directory (except a main worktree's own `.git`, which is masked), a writable root
    /// containing the run records directory, a read-only tree with private git and a main
    /// worktree with private git. A read-only tree is not a writable root.
    pub fn build(declaration: &JobDeclaration, host: &HostGit) -> Result<Self, ConfineError> {
        let mut roots = vec![canonical(&declaration.scratch)?];
        let mut writable_trees = Vec::new();
        for tree in &declaration.trees {
            if tree.access == TreeAccess::ReadOnly && tree.git == TreeGit::Private {
                return Err(ConfineError::ReadOnlyTreeWithPrivateGit {
                    tree: tree.path.clone(),
                });
            }
            let path = canonical(&tree.path)?;
            if tree.access == TreeAccess::Writable {
                roots.push(path.clone());
                writable_trees.push((path, tree.git));
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
            if let Some(root) = roots.iter().find(|root| records_dir.starts_with(root)) {
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
            masks.push(dot_git.clone());
            masks.extend(nested_worktrees);
            masks.extend(nested_repositories.iter().cloned());
            let mut excluded = vec![dot_git];
            excluded.extend(nested_repositories);
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
        })
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
