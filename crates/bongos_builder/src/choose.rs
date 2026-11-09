//! Which proposal a command acts on: the newest pending one of the task at point; with none
//! pending, the newest; with several pending, one picked from a list. `task: edit proposal` opens
//! the chosen file as a buffer; `task: show proposal` shows it in the proposal view.

use std::path::PathBuf;

use bongos_builder_model::apply::file_hash;
use bongos_work::model::layout::{PROPOSALS_DIR, proposal_file};
use bongos_work::model::store::Task;
use bongos_work::{Then, WorkStore, pick, ready, task_at_point, with_task};
use gpui::{Context, Window};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::actions::show_toast;

/// The proposal a command acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chosen {
    /// The task whose directory holds the file.
    pub task_id: String,
    pub number: u32,
    pub path: PathBuf,
}

/// One proposal file of a task and whether it is waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub number: u32,
    pub path: PathBuf,
    /// Not applied or rejected as it is now: no record of its number, or the file's hash is not
    /// the record's.
    pub pending: bool,
}

/// What to do with the candidates of a task.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// The task has no proposal file.
    Nothing,
    One(Candidate),
    /// Several are pending: ask which, newest first.
    Pick(Vec<Candidate>),
}

/// The newest pending candidate; with none pending, the newest; with several pending, a picker.
pub(crate) fn choose(mut candidates: Vec<Candidate>) -> Choice {
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.number));
    let pending: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.pending)
        .cloned()
        .collect();
    match pending.len() {
        0 => candidates
            .into_iter()
            .next()
            .map_or(Choice::Nothing, Choice::One),
        1 => Choice::One(pending.into_iter().next().expect("one pending")),
        _ => Choice::Pick(pending),
    }
}

/// The numbers of the `<n>.proposal.txt` files the store lists for `task`.
fn numbers(task: &Task) -> Vec<u32> {
    task.files
        .keys()
        .filter(|path| path.starts_with(PROPOSALS_DIR))
        .filter_map(|path| {
            path.file_name()?
                .to_str()?
                .strip_suffix(".proposal.txt")?
                .parse()
                .ok()
        })
        .collect()
}

/// Whether `task` has a proposal file.
pub(crate) fn has_proposals(task: &Task) -> bool {
    !numbers(task).is_empty()
}

/// The proposal files of `task` with their state. Reads each file to hash it, so it belongs on a
/// background thread; a file that cannot be read is left out.
pub(crate) fn read_candidates(task: &Task) -> Vec<Candidate> {
    let records = task.file.as_ref().map(|file| file.proposals.as_slice());
    numbers(task)
        .into_iter()
        .filter_map(|number| {
            let path = proposal_file(&task.dir, number);
            let bytes = std::fs::read(&path).ok()?;
            let hash = file_hash(&bytes);
            let pending = !records
                .unwrap_or_default()
                .iter()
                .rev()
                .find(|record| record.number == number)
                .is_some_and(|record| record.hash == hash);
            Some(Candidate {
                number,
                path,
                pending,
            })
        })
        .collect()
}

/// Finds the proposal to act on, asking for the task (when there is none at point) and for the
/// proposal (when several are pending), then calls `then`. A toast says why when there is none.
pub(crate) fn with_proposal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: Then<Chosen>,
) {
    if ready(workspace, cx).is_none() {
        return;
    }
    match task_at_point(workspace, window, cx) {
        Some(id) => choose_in(id, workspace, window, cx, then),
        None => with_task(
            workspace,
            window,
            cx,
            "Which task's proposal?",
            "No task has a proposal",
            |task, _| has_proposals(task),
            Box::new(move |id, workspace, window, cx| choose_in(id, workspace, window, cx, then)),
        ),
    }
}

/// Reads the proposals of the task `task_id` and acts on the choice.
fn choose_in(
    task_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: Then<Chosen>,
) {
    let task = WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .and_then(|tree| tree.task_by_id(&task_id).cloned());
    let Some(task) = task else {
        show_toast(workspace, format!("no task has the id {task_id}"), cx);
        return;
    };
    let read = cx
        .background_executor()
        .spawn(async move { read_candidates(&task) });
    cx.spawn_in(window, async move |workspace, cx| {
        let candidates = read.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let chosen = move |candidate: Candidate| Chosen {
                    task_id: task_id.clone(),
                    number: candidate.number,
                    path: candidate.path,
                };
                match choose(candidates) {
                    Choice::Nothing => {
                        show_toast(workspace, "That task has no proposal".to_string(), cx)
                    }
                    Choice::One(candidate) => then(chosen(candidate), workspace, window, cx),
                    Choice::Pick(candidates) => {
                        let labels = candidates
                            .iter()
                            .map(|candidate| format!("proposal {}", candidate.number))
                            .collect();
                        pick(
                            workspace,
                            "Which proposal?",
                            labels,
                            0,
                            Box::new(move |index, workspace, window, cx| {
                                if let Some(candidate) = candidates.into_iter().nth(index) {
                                    then(chosen(candidate), workspace, window, cx);
                                }
                            }),
                            window,
                            cx,
                        );
                    }
                }
            })
            .ok();
    })
    .detach();
}

/// `task: edit proposal`: the chosen file as a buffer, in the `Proposal` language.
pub(crate) fn edit_proposal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_proposal(
        workspace,
        window,
        cx,
        Box::new(|chosen, workspace, window, cx| {
            let open = workspace.open_abs_path(
                chosen.path,
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..OpenOptions::default()
                },
                window,
                cx,
            );
            cx.spawn(async move |workspace, cx| {
                if let Err(error) = open.await {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(
                                workspace,
                                format!("Could not open the proposal: {error}"),
                                cx,
                            )
                        })
                        .ok();
                }
            })
            .detach();
        }),
    );
}
