//! What the injected `context.md` needs besides a `SessionContext`: facts about the task that
//! `bongos_chat` reads from the Work snapshot. It is a plain struct so the renderer stays pure.
//! It carries no artifact path and no artifact description: `notes_dir` is a directory path, never
//! a listing. The only source of artifact paths is `SessionContext::artifacts`.

use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskFacts {
    pub project_title: Option<String>,
    pub project_dir: Option<PathBuf>,
    pub notes_dir: Option<PathBuf>,
    /// The goal and the state label of each task this one depends on.
    pub deps: Vec<(String, String)>,
    /// Pull request links from the task's `[result]`.
    pub pr_links: Vec<String>,
}
