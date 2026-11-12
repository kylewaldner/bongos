//! The repository overview (D13): for each entry of `work.repositories`, where its new worktrees
//! go, what is wrong with that, whether its refs are polled instead of watched, and which tasks
//! still have a worktree under the old `<repository>/.bongos-worktrees/`.
//!
//! [`WorkStore::repository_overview`](crate::WorkStore::repository_overview) holds the result.
//! It reads the file system (permission probes, symbolic links, `statfs`), so [`compute`] runs on
//! the background executor whenever the snapshot or the repository settings change; the UI only
//! reads the cached rows.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work_model::store::WorkTree;
pub use bongos_work_model::worktree_path::Source as RootSource;
use bongos_work_model::worktree_path::{
    MAX_PATH_SLUG, MAX_WORKTREE_PATH, TemplateError, TemplateLayer, preview as preview_path,
};
use fs::Fs;

use crate::Stats;
use crate::ref_watch::{GitDirInfo, git_dir_info};
use crate::settings::Repository;
use crate::worktrees::{
    Layout, WORKTREES_DIR, configured, expand_home, inside_line, normalize, probe_writable,
};

/// The placeholder [`preview_path`] puts where the task's slug goes.
const PREVIEW_SLUG: &str = "<task>";

/// What the Repositories screen shows for one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryRow {
    pub name: String,
    /// The primary checkout, with `~` expanded.
    pub path: PathBuf,
    /// Which setting decides the root.
    pub root_source: RootSource,
    /// The template text in force; `None` for the central directory.
    pub template: Option<String>,
    /// Where a new worktree goes, with `<task>` where the slug goes ("New worktrees go to:
    /// <preview>"). Empty when the template is invalid.
    pub preview: String,
    /// The first thing wrong with the root, if anything.
    pub problem: Option<RootProblem>,
    /// Whether the repository's refs are checked on a timer because its file system delivers no
    /// change notifications (D10).
    pub slow_poll: bool,
    /// Tasks whose recorded worktree is under `<path>/.bongos-worktrees/` and still exists, as
    /// `(task id, slug)`.
    pub old_root: Vec<(String, String)>,
}

/// What is wrong with a repository's worktree root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootProblem {
    /// The template does not parse (unknown token, unbalanced brace, `..`).
    Template(TemplateError),
    /// A worktree path with a slug of [`MAX_PATH_SLUG`] characters is over [`MAX_WORKTREE_PATH`]
    /// characters.
    TooLong { length: usize },
    /// Files cannot be made in the parent directory (or, while it does not exist yet, in its
    /// nearest existing ancestor `directory`).
    Unwritable { directory: PathBuf },
    /// The root is inside the checkout; its top directory is added to `info/exclude`.
    InsideCheckout,
}

impl fmt::Display for RootProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootProblem::Template(error) => write!(f, "{error}"),
            RootProblem::TooLong { length } => write!(
                f,
                "a worktree path would be {length} characters, over {MAX_WORKTREE_PATH}"
            ),
            RootProblem::Unwritable { directory } => {
                write!(f, "{} is not writable", directory.display())
            }
            RootProblem::InsideCheckout => write!(f, "the root is inside the checkout"),
        }
    }
}

/// A task whose worktree is under a repository's old `.bongos-worktrees/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OldRootTask {
    pub id: String,
    pub slug: String,
    /// The recorded worktree, normalized.
    pub worktree: PathBuf,
}

/// For each of `repositories` (in order), the tasks of any status whose recorded worktree
/// normalizes to a path under `<repository>/.bongos-worktrees/` (D8). It does not look at
/// whether the directories exist. Paths are resolved through symbolic links as far as they
/// exist, so this reads the file system: run it on the background executor.
pub fn old_root_tasks(tree: &WorkTree, repositories: &[Repository]) -> Vec<Vec<OldRootTask>> {
    let roots: Vec<PathBuf> = repositories
        .iter()
        .map(|repository| normalize(&expand_home(&repository.path)).join(WORKTREES_DIR))
        .collect();
    let mut found: Vec<Vec<OldRootTask>> = repositories.iter().map(|_| Vec::new()).collect();
    for task in tree.tasks() {
        let (Some(id), Some(worktree)) = (
            task.id(),
            task.file.as_ref().and_then(|file| file.worktree.as_deref()),
        ) else {
            continue;
        };
        let worktree = normalize(&expand_home(worktree));
        for (root, tasks) in roots.iter().zip(&mut found) {
            if worktree.starts_with(root) && worktree != *root {
                tasks.push(OldRootTask {
                    id: id.to_string(),
                    slug: task.slug.clone(),
                    worktree: worktree.clone(),
                });
            }
        }
    }
    found
}

/// Everything [`compute`] needs, copied out of the store so it can run on another thread.
pub(crate) struct Inputs {
    pub(crate) repositories: Vec<Repository>,
    pub(crate) layout: Layout,
    pub(crate) tree: Option<Arc<WorkTree>>,
    /// The git dirs the store already knows, by repository path.
    pub(crate) known: HashMap<PathBuf, GitDirInfo>,
    pub(crate) poll_decider: fn(&Path) -> bool,
}

/// The rows for `inputs`, and the git dirs it had to look up (for the store's cache). Blocking
/// file system calls: run it on the background executor.
pub(crate) async fn compute(
    fs: &dyn Fs,
    inputs: Inputs,
) -> (Vec<RepositoryRow>, HashMap<PathBuf, GitDirInfo>) {
    let old = match &inputs.tree {
        Some(tree) => old_root_tasks(tree, &inputs.repositories),
        None => inputs.repositories.iter().map(|_| Vec::new()).collect(),
    };
    let mut learned = HashMap::new();
    let mut rows = Vec::new();
    for (repository, old) in inputs.repositories.iter().zip(old) {
        let path = expand_home(&repository.path);
        let info = match inputs.known.get(&path) {
            Some(info) => Some(info.clone()),
            None => {
                let info = git_dir_info(fs, &path, inputs.poll_decider).await;
                if let Some(info) = &info {
                    learned.insert(path, info.clone());
                }
                info
            }
        };
        let old_root = old
            .into_iter()
            .filter(|task| task.worktree.exists())
            .map(|task| (task.id, task.slug))
            .collect();
        let slow_poll = info.is_some_and(|info| info.slow_poll);
        rows.push(row(repository, &inputs.layout, old_root, slow_poll));
    }
    (rows, learned)
}

/// The row of `repository`. Probes the file system for the writability of the root.
pub(crate) fn row(
    repository: &Repository,
    layout: &Layout,
    old_root: Vec<(String, String)>,
    slow_poll: bool,
) -> RepositoryRow {
    let template = configured(repository, layout);
    let root_source = match template {
        Some(configured) => match configured.layer {
            TemplateLayer::Own => RootSource::Own,
            TemplateLayer::Default => RootSource::Default,
        },
        None => RootSource::Central,
    };
    let path = expand_home(&repository.path);
    let rendered = preview_path(
        template.map(|configured| configured.text),
        &repository.name,
        Path::new(&repository.path),
        &layout.home,
        &layout.central,
    );
    let (preview, problem) = match rendered {
        Err(error) => (String::new(), Some(RootProblem::Template(error))),
        Ok(preview) => {
            let problem = problem_of(&preview, &path);
            (preview, problem)
        }
    };
    RepositoryRow {
        name: repository.name.clone(),
        path,
        root_source,
        template: template.map(|configured| configured.text.to_string()),
        preview,
        problem,
        slow_poll,
        old_root,
    }
}

/// The first problem of the worktree path `preview` for the checkout at `repository`: a path
/// too long for the longest slug, a parent that cannot be written, a root inside the checkout.
fn problem_of(preview: &str, repository: &Path) -> Option<RootProblem> {
    let slugs = preview.matches(PREVIEW_SLUG).count();
    let length = preview.chars().count() - slugs * PREVIEW_SLUG.len() + slugs * MAX_PATH_SLUG;
    if length > MAX_WORKTREE_PATH {
        return Some(RootProblem::TooLong { length });
    }
    let worktree = Path::new(preview);
    if let Some(parent) = worktree.parent() {
        let ancestor = parent.ancestors().find(|ancestor| ancestor.exists());
        if let Some(directory) = ancestor
            && !is_writable_directory(directory)
        {
            return Some(RootProblem::Unwritable {
                directory: directory.to_path_buf(),
            });
        }
    }
    inside_line(repository, worktree).map(|_| RootProblem::InsideCheckout)
}

/// Whether files can be made in `directory`: it is a directory, its owner's write bit is set
/// (a read-only directory is refused even for a user who could override the bits), and a probe
/// file can be made in it.
fn is_writable_directory(directory: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(directory) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o200 == 0 {
            return false;
        }
    }
    probe_writable(directory).is_ok()
}

/// The text `work: dump repositories` writes: the counters of `stats`, then every row.
pub(crate) fn dump_text(rows: &[RepositoryRow], stats: &Stats) -> String {
    let mut text = String::new();
    let Stats {
        loads,
        event_reloads,
        ignored_batches,
        writes,
        retries,
        rebase_probes,
        ref_events,
    } = stats;
    writeln!(text, "stats").ok();
    for (name, value) in [
        ("loads", loads),
        ("event_reloads", event_reloads),
        ("ignored_batches", ignored_batches),
        ("writes", writes),
        ("retries", retries),
        ("rebase_probes", rebase_probes),
        ("ref_events", ref_events),
    ] {
        writeln!(text, "  {name}: {value}").ok();
    }
    for row in rows {
        writeln!(text, "repository {}", row.name).ok();
        writeln!(text, "  path: {}", row.path.display()).ok();
        let source = match row.root_source {
            RootSource::Own => "own",
            RootSource::Default => "default",
            RootSource::Central => "central",
        };
        writeln!(
            text,
            "  root: {source} {}",
            row.template.as_deref().unwrap_or("-")
        )
        .ok();
        writeln!(text, "  new worktrees go to: {}", row.preview).ok();
        if let Some(problem) = &row.problem {
            writeln!(text, "  problem: {problem}").ok();
        }
        writeln!(text, "  slow poll: {}", row.slow_poll).ok();
        for (id, slug) in &row.old_root {
            writeln!(text, "  old root: {slug} ({id})").ok();
        }
    }
    text
}
