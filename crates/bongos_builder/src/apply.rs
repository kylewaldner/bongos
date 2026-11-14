//! `task: apply proposal` and `task: reject proposal` (D7, D8).
//!
//! The apply resolves the proposal, saves its open buffer (refusing a buffer in conflict with the
//! file), reads the file, and stops on any error by showing the proposal view at the first
//! problem. Otherwise it collects the running work, computes the diff on the background, asks for
//! the keeper of every split whose keeper the proposal does not mark, opens the preview and asks
//! "Apply proposal N?". Nothing is written before that answer, except the save of the buffer.
//!
//! On Apply one command goes to the work store's serial writer; in a project repository it makes
//! the working copy `proposal-<root slug>-<n>`, and the Apply is the person's `project: approve`
//! of it (see [`apply_and_approve`]). It runs on the tree the writer
//! holds, with the running work collected and the file read when Apply was chosen (never the
//! previewed ones), and refuses unless the diff it computes has the previewed fingerprint; so a
//! tree, file or running chat that changed since the preview writes nothing. A proposal applied
//! as it is (its root records it with the file's hash) is refused, so applying twice never
//! creates twice. The apply never stops a process: a task with a running chat is kept, not
//! cancelled.
//!
//! A reject records the rejection in the root's `[[proposals]]` record, through the same writer,
//! and abandons the proposal's working copy, if one waits to land.

use std::collections::BTreeSet;
use std::sync::Arc;

use bongos_builder_model::apply::{Request, file_hash, reject};
use bongos_builder_model::diff::{Keeper, KeeperSource, fingerprint};
use bongos_builder_model::{
    Choices, Diff, Node, Op, Parsed, Severity, applied_summary, diff, preview,
};
use bongos_work::model::PersonAtKeyboard;
use bongos_work::model::commands::{Context as WorkContext, Outcome};
use bongos_work::model::store::WorkTree;
use bongos_work::model::{ProposalState, TaskId};
use bongos_work::{
    LandingVerdict, LandingWatch, WorkStore, WriteError, WriteOutcome, pick, ready,
};
use gpui::{App, AppContext as _, Context, Entity, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::actions::show_toast;
use crate::choose::{self, Chosen};
use crate::{TaskBuilderSettings, later, live, preview_view, view};

/// The question before anything is written.
pub(crate) fn confirm_prompt(number: u32) -> String {
    format!("Apply proposal {number}?")
}

/// The answers to [`confirm_prompt`], in order.
pub(crate) const APPLY: &str = "Apply";
pub(crate) const CANCEL: &str = "Cancel";

/// The toast of a diff with no operation.
pub(crate) fn nothing_message(number: u32) -> String {
    format!("Nothing to change: the tree already matches proposal {number}")
}

/// The refusal of a proposal its root records as applied with the file's present hash.
pub(crate) fn already_applied_message(number: u32) -> String {
    format!("Proposal {number} is already applied as it is; edit it or write the next one")
}

/// The refusal of an open buffer in conflict with the file on disk.
pub(crate) fn conflict_message(number: u32) -> String {
    format!(
        "The buffer of proposal {number} conflicts with its file on disk; reload or save it \
         first. Nothing was applied"
    )
}

/// The toast of a proposal with an error.
pub(crate) fn errors_message(number: u32) -> String {
    format!("Proposal {number} has errors; nothing was applied")
}

/// The toast of a reject.
pub(crate) fn rejected_message(number: u32) -> String {
    format!("Proposal {number} rejected; its file stays")
}

/// Whether the task `root` records proposal `number` as applied with the file hash `hash`.
pub(crate) fn applied_as_is(tree: &WorkTree, root: &str, number: u32, hash: &str) -> bool {
    tree.task_by_id(root)
        .and_then(|task| task.file.as_ref())
        .is_some_and(|file| {
            file.proposals.iter().any(|record| {
                record.number == number
                    && record.state == ProposalState::Applied
                    && record.hash == hash
            })
        })
}

/// The writer's command of an apply: refused when the proposal is already applied as it is, else
/// [`Request::run`] with the running work `live` and the file's `bytes` collected when Apply was
/// chosen. `root` is the proposal's root task.
pub(crate) fn apply_command(
    request: &Request,
    root: &str,
    number: u32,
    tree: &WorkTree,
    ctx: &WorkContext,
    live: &bongos_builder_model::LiveSet,
    bytes: &[u8],
) -> Outcome {
    if applied_as_is(tree, root, number, &file_hash(bytes)) {
        return Err(already_applied_message(number));
    }
    request.run(tree, ctx, live, bytes)
}

/// Whether the task `root` records proposal `number` as applied, whatever the file's hash.
pub(crate) fn ever_applied(tree: &WorkTree, root: &str, number: u32) -> bool {
    tree.task_by_id(root)
        .and_then(|task| task.file.as_ref())
        .is_some_and(|file| {
            file.proposals
                .iter()
                .any(|record| record.number == number && record.state == ProposalState::Applied)
        })
}

/// The refusal of a reject of an applied proposal.
pub(crate) fn reject_applied_message(number: u32) -> String {
    format!("Proposal {number} is applied; a reject cannot take it back")
}

/// The writer's command of a reject: the record of proposal `number` on the task `root`, with
/// the hash of `bytes`. A proposal applied once is never rejected, even after its file was
/// edited: the tree already holds it, and the reject would replace its applied record.
pub(crate) fn reject_command(
    root: &str,
    number: u32,
    tree: &WorkTree,
    ctx: &WorkContext,
    bytes: &[u8],
) -> Outcome {
    if ever_applied(tree, root, number) {
        return Err(reject_applied_message(number));
    }
    reject(tree, ctx, root, number, &file_hash(bytes))
}

/// `task: apply proposal`.
pub(crate) fn apply_proposal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    choose::with_proposal(workspace, window, cx, Box::new(save_and_read));
}

/// Saves the open buffer of the chosen file when it is dirty (refusing one in conflict with the
/// disk), then reads the file as the work store's file system has it.
fn save_and_read(
    chosen: Chosen,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let number = chosen.number;
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    };
    let fs = store.read(cx).fs();
    let project = workspace.project().clone();
    let save = match view::open_buffer(&project, &chosen.path, cx) {
        Some(buffer) if buffer.read(cx).has_conflict() => {
            show_toast(workspace, conflict_message(number), cx);
            return;
        }
        Some(buffer) if buffer.read(cx).is_dirty() => {
            Some(project.update(cx, |project, cx| project.save_buffer(buffer, cx)))
        }
        _ => None,
    };
    cx.spawn_in(window, async move |workspace, cx| {
        if let Some(save) = save
            && let Err(error) = save.await
        {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!("Could not save proposal {number}: {error}. Nothing was applied"),
                        cx,
                    )
                })
                .ok();
            return;
        }
        let bytes = fs.load_bytes(&chosen.path).await;
        workspace
            .update_in(cx, |workspace, window, cx| match bytes {
                Ok(bytes) => check(chosen, bytes, workspace, window, cx),
                Err(error) => show_toast(
                    workspace,
                    format!("Could not read proposal {number}: {error}"),
                    cx,
                ),
            })
            .ok();
    })
    .detach();
}

/// An apply between the read of the file and the confirm.
struct Pending {
    chosen: Chosen,
    parsed: Arc<Parsed>,
    tree: Arc<WorkTree>,
    /// The running work the preview is computed with.
    live: bongos_builder_model::LiveSet,
    /// The hash of the file as read.
    hash: String,
    choices: Choices,
    /// The split leaves whose keeper was asked.
    asked: BTreeSet<TaskId>,
}

/// Parses and validates the file: with an error, the proposal view opens on the first problem
/// and nothing is applied. Otherwise the diff is computed.
fn check(
    chosen: Chosen,
    bytes: Vec<u8>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let number = chosen.number;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        show_toast(
            workspace,
            format!("Proposal {number} is not UTF-8 text; nothing was applied"),
            cx,
        );
        return;
    };
    let live = live::collect(&tree, cx);
    let analysis = view::analyze(
        text,
        number,
        &chosen.task_id,
        &tree,
        &live,
        TaskBuilderSettings::get_global(cx).todo_warning,
        live::workflow_names(cx),
    );
    if analysis
        .problems
        .iter()
        .any(|problem| problem.severity == Severity::Error)
    {
        show_toast(workspace, errors_message(number), cx);
        view::show_problems(chosen, workspace, window, cx);
        return;
    }
    let hash = file_hash(&bytes);
    // Validation made the root's id the id of the task that holds the file.
    if applied_as_is(&tree, &chosen.task_id, number, &hash) {
        show_toast(workspace, already_applied_message(number), cx);
        return;
    }
    compute(
        Pending {
            chosen,
            parsed: Arc::new(analysis.parsed),
            tree,
            live,
            hash,
            choices: Choices::default(),
            asked: BTreeSet::new(),
        },
        window,
        cx,
    );
}

/// Computes the diff of the pending apply on the background, then decides what comes next.
fn compute(pending: Pending, window: &mut Window, cx: &mut Context<Workspace>) {
    let parsed = pending.parsed.clone();
    let tree = pending.tree.clone();
    let live = pending.live.clone();
    let choices = pending.choices.clone();
    let task = cx.background_spawn(async move { diff(&parsed, &tree, &live, &choices) });
    cx.spawn_in(window, async move |workspace, cx| {
        let diff = task.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                decide(pending, diff, workspace, window, cx)
            })
            .ok();
    })
    .detach();
}

/// A refused diff stops with a toast; a split with an unmarked keeper asks for it; otherwise the
/// preview opens, and the confirm follows unless there is nothing to change.
fn decide(
    pending: Pending,
    diff: Diff,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let number = pending.chosen.number;
    if let Some(refusal) = &diff.refusal {
        show_toast(
            workspace,
            format!("Proposal {number} cannot be applied: {refusal}"),
            cx,
        );
        return;
    }
    if let Some((id, keeper)) = unasked_keeper(&diff, &pending.asked) {
        ask_keeper(pending, id, keeper, workspace, window, cx);
        return;
    }
    let text = preview(&pending.parsed, &pending.tree, &diff);
    if diff.is_empty() {
        preview_view::open(
            number,
            text,
            workspace,
            window,
            cx,
            Box::new(move |workspace, _, cx| show_toast(workspace, nothing_message(number), cx)),
        );
        return;
    }
    preview_view::open(
        number,
        text,
        workspace,
        window,
        cx,
        Box::new(move |workspace, window, cx| confirm(pending, diff, workspace, window, cx)),
    );
}

/// The first split whose keeper is the default and was not asked yet.
fn unasked_keeper(diff: &Diff, asked: &BTreeSet<TaskId>) -> Option<(TaskId, Keeper)> {
    diff.ops.iter().find_map(|op| match op {
        Op::Split {
            id,
            keeper: Some(keeper),
            ..
        } if keeper.source == KeeperSource::Default && !asked.contains(id) => {
            Some((id.clone(), keeper.clone()))
        }
        _ => None,
    })
}

/// The task line of `id` in the proposal.
fn find<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    if node.attributes.id.as_deref() == Some(id) {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, id))
}

/// Asks which new child of the split leaf `id` keeps its work, the default selected, then
/// computes the diff again with the answer.
fn ask_keeper(
    mut pending: Pending,
    id: TaskId,
    keeper: Keeper,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let labels: Vec<String> = find(&pending.parsed.root, &id)
        .map(|node| {
            node.children
                .iter()
                .filter(|child| child.attributes.id.is_none())
                .map(|child| child.label.clone())
                .collect()
        })
        .unwrap_or_default();
    if labels.is_empty() {
        pending.asked.insert(id);
        compute(pending, window, cx);
        return;
    }
    let selected = labels
        .iter()
        .position(|label| *label == keeper.label)
        .unwrap_or(0);
    let task = pending.tree.task_by_id(&id);
    let question = match task
        .and_then(|task| task.file.as_ref())
        .and_then(|file| file.branch.clone())
    {
        Some(branch) => format!("Which child keeps the branch {branch}?"),
        None => format!(
            "Which child of {} keeps its work?",
            task.map_or(id.as_str(), |task| task.slug.as_str())
        ),
    };
    let choices = labels.clone();
    pick(
        workspace,
        question,
        choices,
        selected,
        Box::new(move |index, _workspace, window, cx| {
            if let Some(label) = labels.get(index) {
                pending.choices.keepers.insert(id.clone(), label.clone());
            }
            pending.asked.insert(id);
            compute(pending, window, cx);
        }),
        window,
        cx,
    );
}

/// Asks "Apply proposal N?"; Apply queues the write, anything else writes nothing.
fn confirm(
    pending: Pending,
    previewed: Diff,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let number = pending.chosen.number;
    pick(
        workspace,
        confirm_prompt(number),
        vec![APPLY.to_string(), CANCEL.to_string()],
        0,
        Box::new(move |index, workspace, window, cx| {
            if index == 0 {
                queue(pending, previewed, workspace, window, cx);
            }
        }),
        window,
        cx,
    );
}

/// Queues the one command of the apply on the writer, with the running work collected and the
/// file read now, at the confirm, and toasts its outcome.
fn queue(
    pending: Pending,
    previewed: Diff,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let number = pending.chosen.number;
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    };
    if ready(workspace, cx).is_none() {
        return;
    }
    let request = Request {
        parsed: (*pending.parsed).clone(),
        choices: pending.choices.clone(),
        fingerprint: fingerprint(&previewed, &pending.choices, &pending.hash),
    };
    let summary = applied_summary(number, &previewed);
    let root = previewed.root;
    let fs = store.read(cx).fs();
    let path = pending.chosen.path;
    cx.spawn_in(window, async move |workspace, cx| {
        // The file as it is now, never the previewed bytes: an edit since the preview changes
        // the hash, and the writer refuses.
        let bytes = match fs.load_bytes(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(
                            workspace,
                            format!(
                                "Could not read proposal {number}: {error}. Nothing was applied"
                            ),
                            cx,
                        )
                    })
                    .ok();
                return;
            }
        };
        let Ok(applying) = workspace.update_in(cx, |_, window, cx| {
            // The running work now, right before the write is queued, never the previewed set:
            // a chat started since the preview (or during the read above) changes the diff, and
            // the writer refuses.
            let now = store.read(cx).snapshot();
            let live = now.map(|now| live::collect(&now, cx)).unwrap_or_default();
            let person = |window: &Window, cx: &App| {
                WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx))
            };
            apply_and_approve(
                &store,
                (person(window, cx), person(window, cx)),
                (request, root, number),
                (live, bytes),
                summary,
                cx,
            )
        }) else {
            return;
        };
        let message = applying.await;
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// Writes the apply as the person `contexts.0`, and approves it as the person `contexts.1` (two
/// attestations: one is spent by the write, one by the approval). In a project repository the
/// write makes the working copy `proposal-<root slug>-<number>` (the author's `proposal` is set
/// here) that waits to land, and `project: approve` of it is the Apply; master is not touched
/// before. Resolves with the toast: `summary` once it applied, or why it did not. A held landing
/// (a failed check) leaves the proposal pending with the reason, which the needs-you source
/// reports.
pub(crate) fn apply_and_approve(
    store: &Entity<WorkStore>,
    contexts: (WorkContext, WorkContext),
    (request, root, number): (Request, TaskId, u32),
    (live, bytes): (bongos_builder_model::LiveSet, Vec<u8>),
    summary: String,
    cx: &mut App,
) -> gpui::Task<String> {
    let (mut ctx, approver) = contexts;
    ctx.author.proposal = Some((root.clone(), number));
    let watch = bongos_work::watch_landings(cx);
    let write = store.update(cx, |store, cx| {
        let task = root.clone();
        store.write_outcome(
            ctx,
            move |tree, ctx| apply_command(&request, &task, number, tree, ctx, &live, &bytes),
            cx,
        )
    });
    cx.spawn(async move |cx| match write.await {
        // Written at once: not a project repository, or the change is state only.
        Ok(WriteOutcome::Applied(_)) => summary,
        Ok(WriteOutcome::Landing { change_id, .. }) => {
            approve_and_follow(watch, &change_id, approver, (root, number), summary, cx).await
        }
        Err(error) => error.to_string(),
    })
}

/// Approves the proposal's working copy `change_id` as the person `approver` and follows its
/// landing with `watch` (made before the write). The landing runs from the write on, so it may
/// have said it waits for approval before the approval was written: once approved, that wait is
/// over, and only a refusal, a conflict or the end of the landing holds it.
pub(crate) async fn approve_and_follow(
    watch: LandingWatch,
    change_id: &str,
    approver: WorkContext,
    (root, number): (TaskId, u32),
    summary: String,
    cx: &mut gpui::AsyncApp,
) -> String {
    let approved = cx.update(|cx| bongos_work::approve_proposal(change_id, approver, cx));
    if let Err(error) = approved.await {
        return format!("Proposal {number} was not approved: {error}");
    }
    match watch.verdict(change_id, true).await {
        LandingVerdict::Landed => summary,
        LandingVerdict::Held(reason) => {
            let held = format!("Proposal {number} pending: {reason}");
            cx.update(|cx| later::hold(&root, number, reason, cx));
            held
        }
        LandingVerdict::Ended(reason) => format!("Proposal {number} was not applied: {reason}"),
    }
}

/// `task: reject proposal`.
pub(crate) fn reject_proposal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    choose::with_proposal(workspace, window, cx, Box::new(reject_chosen));
}

/// Records the chosen proposal as rejected, with the hash of the file as it is on disk.
fn reject_chosen(
    chosen: Chosen,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = WorkStore::global(cx) else {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    };
    let fs = store.read(cx).fs();
    cx.spawn_in(window, async move |workspace, cx| {
        let Chosen {
            task_id,
            number,
            path,
        } = chosen;
        let bytes = match fs.load_bytes(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(
                            workspace,
                            format!("Could not read proposal {number}: {error}"),
                            cx,
                        )
                    })
                    .ok();
                return;
            }
        };
        let Ok(rejecting) = workspace.update_in(cx, |_, window, cx| {
            let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
            let root = task_id.clone();
            reject_and_abandon(
                &store,
                ctx,
                (task_id, number),
                move |tree, ctx| reject_command(&root, number, tree, ctx, &bytes),
                cx,
            )
        }) else {
            return;
        };
        let message = rejecting.await;
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// Writes the rejection `command` of proposal `number` of `root` as `ctx`; only once that
/// succeeded is the working copy of the proposal, if one waits to land, abandoned with its branch
/// and files. A rejection that was not written leaves it waiting. Master is never touched.
/// Resolves with the toast.
pub(crate) fn reject_and_abandon(
    store: &Entity<WorkStore>,
    ctx: WorkContext,
    (root, number): (TaskId, u32),
    command: impl Fn(&WorkTree, &WorkContext) -> Outcome + Send + 'static,
    cx: &mut App,
) -> gpui::Task<String> {
    let write = store.update(cx, |store, cx| store.write(ctx, command, cx));
    cx.spawn(async move |cx| {
        if let Err(error) = write.await {
            return error.to_string();
        }
        let abandon = cx.update(|cx| bongos_work::abandon_proposal(&root, number, cx));
        match abandon.await {
            Ok(_) => rejected_message(number),
            Err(error) => format!("Proposal {number} rejected, but its working copy: {error}"),
        }
    })
}
