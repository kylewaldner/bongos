//! What a chat is told about proposals: one section at the end of its `context.md` (and of the
//! Codex instructions), added through the chats' context extension while `task_builder.enabled`
//! is true.
//!
//! A chat of type `plan` gets the splitting test, the format, the rules in one paragraph, where
//! to write the proposal and the task's subtree printed as a proposal. Every other chat gets a
//! three-line pointer to the format. The section holds only what the chat's own task subtree
//! already shows (goals, kinds, todos, edges, planned results and ids) and never an artifact: no
//! artifact path, description or file name, so never a human-only file.

use std::path::Path;

use bongos_builder_model::help::FORMAT;
use bongos_builder_model::{LiveSet, next_number, print};
use bongos_chat::ContextExtension;
use bongos_work::WorkStore;
use bongos_work::model::layout::proposal_file;
use bongos_work::model::store::Task;
use gpui::App;
use settings::Settings as _;

use crate::{BuilderDirs, TaskBuilderSettings};

/// The session type that plans: the chats `task: plan` starts.
pub const PLAN_TYPE: &str = "plan";

/// The longest printed subtree a plan chat's section holds, in lines. A longer one is replaced by
/// a pointer to `task: propose`, which writes it to a file.
pub const MAX_PRINTED_LINES: usize = 200;

/// The context extension of the task builder.
pub(crate) struct BuilderContext {
    dirs: BuilderDirs,
}

/// Installs the context extension of the builder. It adds nothing while `task_builder.enabled` is
/// false.
pub(crate) fn install(dirs: BuilderDirs, cx: &mut App) {
    bongos_chat::set_context_extension(BuilderContext { dirs }, cx);
}

impl ContextExtension for BuilderContext {
    fn section(&self, chat_type: &str, task_id: Option<&str>, cx: &App) -> Option<String> {
        if !TaskBuilderSettings::get_global(cx).enabled {
            return None;
        }
        let format_path = self.dirs.format_path();
        if chat_type != PLAN_TYPE {
            return Some(pointer_section(&format_path));
        }
        let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
        let Some(task) = task_id.and_then(|id| tree.task_by_id(id)) else {
            return Some(pointer_section(&format_path));
        };
        let printed = task_id.and_then(|id| print(&tree, id, &LiveSet::default()));
        Some(plan_section(
            &format_path,
            task,
            next_number(task),
            printed.as_deref(),
        ))
    }
}

/// The section of a chat that is not planning: where the format is and what applies at once.
pub fn pointer_section(format_path: &Path) -> String {
    format!(
        "## Task builder\n\n\
         If you find new work with its own result, or a task worth splitting, write a proposal \
         in the task's `proposals/` directory; the format is in {}.\n\
         Changes inside your own task apply at once and are logged; anything else waits for Kyle.\n",
        format_path.display()
    )
}

/// The section of a plan chat on `task`: the splitting test, the rules, where to write proposal
/// `number`, the format, and `printed` (the subtree as a proposal) when it is at most
/// [`MAX_PRINTED_LINES`] long, else a pointer to `task: propose`.
pub fn plan_section(format_path: &Path, task: &Task, number: u32, printed: Option<&str>) -> String {
    let path = proposal_file(&task.dir, number);
    let subtree = match printed {
        Some(text) if text.lines().count() <= MAX_PRINTED_LINES => format!(
            "The task and everything under it now, printed as a proposal you can edit:\n\n\
             ````\n{}````\n",
            terminated(text)
        ),
        _ => format!(
            "The subtree of this task is too long to print here (more than {MAX_PRINTED_LINES} \
             lines). Kyle runs `task: propose` to write a proposal file pre-filled with it; edit \
             that file instead of starting over.\n"
        ),
    };
    format!(
        "## Task builder\n\n\
         You are planning this task. Do not change task files yourself: write a proposal, and \
         Kyle previews and applies it.\n\n\
         Splitting test: anything with its own result, a PR, a document or a spike's findings, \
         is a task; anything on the way to that result, implement, review, write tests, fix CI, \
         is a todo of that task. Do not make a leaf for tests unless they are their own PR. A \
         leaf names one result.\n\n\
         Rules: a proposal has one root task, with children and edges below it. A task with an \
         id is an existing task and a line without one is new. A cycle, an edge between tasks \
         at different levels and two results on a leaf are errors, and a proposal with an error \
         is not applied. Done and cancelled tasks, running work and started todos are never \
         disturbed: a line that would change them is kept as it is and said so in the preview.\n\n\
         Where to write: {path}. Use the next free number, never overwrite an existing file, and \
         put `by chat $BONGOS_CHAT_ID` in the header.\n\n\
         The format, also in {format}:\n\n\
         ````\n{FORMAT}````\n\n\
         {subtree}",
        path = path.display(),
        format = format_path.display(),
    )
}

fn terminated(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}
