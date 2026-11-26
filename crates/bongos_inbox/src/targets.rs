//! Whether an item's `target` still exists. A target is an absolute path, a path relative to an
//! open worktree, or `<scheme>:<rest>` handled by the resolver registered for the scheme. The
//! built-in `task:` and `run:` resolvers look in the work tree: a task id absent from it, or a run
//! with no record under any task's `runs/`, is stale. A scheme with no resolver, a relative path
//! with no open worktree, and `task:` or `run:` before the work tree loads are unresolved, which
//! is not stale.
//!
//! A path target may end in `:<line>`, which opens it at that line. A relative path is looked for
//! in each open worktree root in turn and the first match wins. `<name>:<digits>` where the name
//! has no resolver is read as a relative path and a line, so `plan.md:12` works.
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

    /// Finds `target` now: a relative path in the first open worktree that has it, looking in
    /// `first_roots` before the roots of the open windows.
    pub fn locate(&self, target: &str, first_roots: &[PathBuf], cx: &App) -> Located {
        let known = |scheme: &str| {
            scheme == TASK_SCHEME || scheme == RUN_SCHEME || self.schemes.contains_key(scheme)
        };
        match parse(target, &known) {
            Parsed::Path { path, line } if path.exists() => Located::Path { path, line },
            Parsed::Path { .. } => Located::Gone,
            Parsed::Relative { path, line } => {
                // The window an action runs in is being updated, so the open worktrees cannot be
                // read from it: the caller names its own first.
                let mut roots = first_roots.to_vec();
                roots.extend((self.worktree_roots)(cx));
                if roots.is_empty() {
                    return Located::Unresolved(format!(
                        "no worktree is open to look for {} in",
                        path.display()
                    ));
                }
                match roots
                    .iter()
                    .map(|root| root.join(&path))
                    .find(|p| p.exists())
                {
                    Some(path) => Located::Path { path, line },
                    None => Located::Gone,
                }
            }
            Parsed::Scheme { scheme, rest } => match scheme.as_str() {
                TASK_SCHEME => Located::Task(rest),
                RUN_SCHEME => Located::Run(rest),
                _ => Located::Other { scheme, rest },
            },
            Parsed::Unknown { scheme } => {
                Located::Unresolved(format!("nothing resolves {scheme}: targets"))
            }
        }
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
        match parse(target, &|scheme| self.schemes.contains_key(scheme)) {
            Parsed::Path { path, .. } => exists(&path),
            Parsed::Relative { path, .. } => {
                if self.roots.is_empty() {
                    TargetState::Unresolved
                } else if self.roots.iter().any(|root| root.join(&path).exists()) {
                    TargetState::Live
                } else {
                    TargetState::Stale
                }
            }
            Parsed::Scheme { scheme, rest } => self
                .schemes
                .get(&scheme)
                .map_or(TargetState::Unresolved, |resolve| resolve(&rest)),
            Parsed::Unknown { .. } => TargetState::Unresolved,
        }
    }
}

/// How a target reads, before anything looks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// An absolute path, or one under `~/`.
    Path { path: PathBuf, line: Option<u32> },
    /// A path to look for under the open worktrees.
    Relative { path: PathBuf, line: Option<u32> },
    /// `<scheme>:<rest>` with a resolver for the scheme.
    Scheme { scheme: String, rest: String },
    /// `<scheme>:<rest>` with no resolver for the scheme.
    Unknown { scheme: String },
}

/// Reads `target`. `is_scheme` says whether a resolver exists for a scheme.
pub(crate) fn parse(target: &str, is_scheme: &dyn Fn(&str) -> bool) -> Parsed {
    let (body, line) = split_line(target);
    if let Some(rest) = body.strip_prefix("~/") {
        return Parsed::Path {
            path: util::paths::home_dir().join(rest),
            line,
        };
    }
    if Path::new(body).is_absolute() {
        return Parsed::Path {
            path: PathBuf::from(body),
            line,
        };
    }
    let scheme = split_scheme(target);
    if let Some((scheme, rest)) = scheme
        && is_scheme(scheme)
    {
        return Parsed::Scheme {
            scheme: scheme.to_string(),
            rest: rest.to_string(),
        };
    }
    if line.is_none()
        && let Some((scheme, _)) = scheme
    {
        return Parsed::Unknown {
            scheme: scheme.to_string(),
        };
    }
    Parsed::Relative {
        path: PathBuf::from(body),
        line,
    }
}

/// `target` without a trailing `:<line>`, and the line.
fn split_line(target: &str) -> (&str, Option<u32>) {
    match target.rsplit_once(':') {
        Some((body, line)) if !body.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => {
            match line.parse() {
                Ok(line) => (body, Some(line)),
                Err(_) => (target, None),
            }
        }
        _ => (target, None),
    }
}

/// Where a target is, found now rather than at the last scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    /// An existing file or directory, at `line` when the target named one.
    Path { path: PathBuf, line: Option<u32> },
    /// The task `<id>`.
    Task(String),
    /// The run `<id>`.
    Run(String),
    /// `<scheme>:<rest>` of a scheme registered with [`TargetResolvers::register`].
    Other { scheme: String, rest: String },
    /// The path does not exist.
    Gone,
    /// Nothing can tell; why.
    Unresolved(String),
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
