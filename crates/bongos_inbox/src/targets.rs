//! Whether an item's `target` still exists. A target is an absolute path, a path relative to an
//! open worktree, or `<scheme>:<rest>` handled by the resolver registered for the scheme. The
//! built-in `task:` and `run:` resolvers look in the work tree: a task id absent from it, or a run
//! with no record under any task's `runs/`, is stale. A scheme with no resolver, a relative path
//! with no open worktree, and `task:` or `run:` before the work tree loads are unresolved, which
//! is not stale.
//!
//! Resolvers are prepared on the main thread (where they read app state such as the work tree
//! snapshot) and run on the background executor during a scan.

use bongos_work_model::layout;
use bongos_work_model::store::WorkTree;
use gpui::App;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

pub const TASK_SCHEME: &str = "task";
pub const RUN_SCHEME: &str = "run";

/// What resolving an item's target found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetState {
    /// The target exists.
    Live,
    /// The target is gone.
    Stale,
    /// Nothing can tell: no resolver for the scheme, or nothing to resolve against yet.
    Unresolved,
}

/// Resolves the part of a target after `<scheme>:`. Runs on the background executor.
pub type Resolve = Arc<dyn Fn(&str) -> TargetState + Send + Sync>;
/// Makes a [`Resolve`] from app state, on the main thread, once per scan.
pub type Resolver = Rc<dyn Fn(&App) -> Resolve>;
/// The work tree as the work store last loaded it, `None` before it loads or while it is off.
pub type WorkTreeSource = Rc<dyn Fn(&App) -> Option<Arc<WorkTree>>>;
/// The roots of the open worktrees, which relative path targets resolve against.
pub type WorktreeRoots = Rc<dyn Fn(&App) -> Vec<PathBuf>>;

/// The target resolvers by scheme, plus where the built-in ones look.
pub struct TargetResolvers {
    schemes: HashMap<String, Resolver>,
    work_tree: WorkTreeSource,
    worktree_roots: WorktreeRoots,
}

impl Default for TargetResolvers {
    fn default() -> Self {
        Self {
            schemes: HashMap::new(),
            work_tree: Rc::new(|_| None),
            worktree_roots: Rc::new(|_| Vec::new()),
        }
    }
}

impl TargetResolvers {
    /// Resolves `<scheme>:` targets with `resolver`, replacing any earlier one, built-ins included.
    pub fn register(&mut self, scheme: &str, resolver: impl Fn(&App) -> Resolve + 'static) {
        self.schemes.insert(scheme.to_string(), Rc::new(resolver));
    }

    /// Where the built-in `task:` and `run:` resolvers read the work tree.
    pub fn set_work_tree(&mut self, source: impl Fn(&App) -> Option<Arc<WorkTree>> + 'static) {
        self.work_tree = Rc::new(source);
    }

    /// Where relative path targets find the open worktrees.
    pub fn set_worktree_roots(&mut self, roots: impl Fn(&App) -> Vec<PathBuf> + 'static) {
        self.worktree_roots = Rc::new(roots);
    }

    pub(crate) fn prepare(&self, cx: &App) -> PreparedResolvers {
        let tree = (self.work_tree)(cx);
        let mut schemes: HashMap<String, Resolve> = HashMap::new();
        schemes.insert(TASK_SCHEME.to_string(), task_resolver(tree.clone()));
        schemes.insert(RUN_SCHEME.to_string(), run_resolver(tree));
        for (scheme, resolver) in &self.schemes {
            schemes.insert(scheme.clone(), resolver(cx));
        }
        PreparedResolvers {
            schemes,
            roots: (self.worktree_roots)(cx),
        }
    }
}

/// The resolvers of one scan, ready for the background executor.
pub(crate) struct PreparedResolvers {
    schemes: HashMap<String, Resolve>,
    roots: Vec<PathBuf>,
}

impl PreparedResolvers {
    pub(crate) fn resolve(&self, target: &str) -> TargetState {
        let path = Path::new(target);
        if path.is_absolute() {
            return exists(path);
        }
        if let Some((scheme, rest)) = split_scheme(target) {
            return self
                .schemes
                .get(scheme)
                .map_or(TargetState::Unresolved, |resolve| resolve(rest));
        }
        if self.roots.is_empty() {
            return TargetState::Unresolved;
        }
        if self.roots.iter().any(|root| root.join(path).exists()) {
            TargetState::Live
        } else {
            TargetState::Stale
        }
    }
}

fn exists(path: &Path) -> TargetState {
    if path.exists() {
        TargetState::Live
    } else {
        TargetState::Stale
    }
}

/// `<scheme>:<rest>` where the scheme is a letter followed by letters, digits, `+`, `-` or `.`.
fn split_scheme(target: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = target.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some((scheme, rest))
}

fn task_resolver(tree: Option<Arc<WorkTree>>) -> Resolve {
    Arc::new(move |id| match &tree {
        None => TargetState::Unresolved,
        Some(tree) if tree.task_by_id(id).is_some() => TargetState::Live,
        Some(_) => TargetState::Stale,
    })
}

fn run_resolver(tree: Option<Arc<WorkTree>>) -> Resolve {
    Arc::new(move |run| match &tree {
        None => TargetState::Unresolved,
        Some(tree) => {
            let found = is_plain_name(run)
                && tree
                    .tasks()
                    .iter()
                    .any(|task| run_record_exists(&layout::runs_dir(&task.dir), run));
            if found {
                TargetState::Live
            } else {
                TargetState::Stale
            }
        }
    })
}

/// A single path component that names nothing above it.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Whether `runs` holds the record of `run`: `runs/<run>` or a file `runs/<run>.<extension>`.
fn run_record_exists(runs: &Path, run: &str) -> bool {
    if runs.join(run).exists() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(runs) else {
        return false;
    };
    let prefix = format!("{run}.");
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
}
