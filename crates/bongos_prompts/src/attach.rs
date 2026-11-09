//! Which chat a prompts editor sends to (design D13), the effective auto-submit (D15) and
//! `prompts: toggle auto-submit`.
//!
//! In the Chats panel's prompts pane it is the chat the panel shows. In the center it is one of
//! the non-archived chats whose prompts file is this file: the one running; with several running,
//! the one picked ("Send to which chat?"), remembered in the mode until the editor closes or that
//! chat ends; with none running, the closed one active last (to offer a resume); else none.
//!
//! Everything the prompts mode asks of the chats goes through [`ChatLink`], so tests can stand in
//! a fake for the chats crate; the app uses [`Chats`], which calls `bongos_chat`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_chat::{ChatPanel, ChatRef, ChatStore, SendError, SendOptions, Sent};
use bongos_work::WorkStore;
use editor::Editor;
use gpui::{App, Context, Entity, Global, Task, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::PromptsSettings;
use crate::actions::show_toast;
use crate::mode::mode_of;

/// The toast of `prompts: toggle auto-submit` with no chat attached.
pub(crate) const NO_CHAT_MESSAGE: &str =
    "No chat is attached; prompts.auto_submit sets the default";
/// The question of the picker among several running chats.
pub(crate) const WHICH_CHAT_QUESTION: &str = "Send to which chat?";

/// What runs after a resume offer's send, with its result; only when a send ran.
pub(crate) type OnSent = Box<dyn FnOnce(Result<Sent, SendError>, &mut Window, &mut App)>;

/// What the prompts mode asks of the chats.
pub(crate) trait ChatLink {
    /// The chat the Chats panel shows, when `editor` is the editor of its prompts pane.
    fn panel_chat(
        &self,
        workspace: &Workspace,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<String>;
    /// The non-archived chats whose prompts file is `path`, running ones first, then the one
    /// active last first.
    fn chats_for_file(&self, path: &Path, cx: &App) -> Vec<ChatRef>;
    /// The auto-submit override of `chat_id`.
    fn auto_submit_override(&self, chat_id: &str, cx: &App) -> Option<bool>;
    fn set_auto_submit(
        &self,
        chat_id: &str,
        value: Option<bool>,
        cx: &mut App,
    ) -> Task<Result<(), String>>;
    fn send(
        &self,
        chat_id: &str,
        text: String,
        options: SendOptions,
        cx: &mut App,
    ) -> Task<Result<Sent, SendError>>;
    /// Asks "Resume <name> and send?"; on yes resumes the chat, sends and calls `on_sent`.
    #[allow(clippy::too_many_arguments)]
    fn offer_resume_and_send(
        &self,
        chat_id: String,
        name: String,
        text: String,
        options: SendOptions,
        on_sent: OnSent,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    );
    /// The id of the task whose directory is `dir`, from the Work snapshot.
    fn task_for_dir(&self, dir: &Path, cx: &App) -> Option<String>;
    fn start_chat_on_task(
        &self,
        task_id: String,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    );
    /// The task or project directory `chat_id` belongs to.
    fn scope_dir(&self, chat_id: &str, cx: &App) -> Option<PathBuf>;
    /// The prompts files `prompts: use file` offers for a chat scoped to `scope_dir`: the
    /// project's, then each task's of the project (or the activity).
    fn prompt_choices(&self, scope_dir: &Path, cx: &App) -> Vec<PromptChoice>;
    /// Writes the prompts file of `chat_id`; `None` goes back to the scope's own file.
    fn set_prompts_file(
        &self,
        chat_id: &str,
        path: Option<PathBuf>,
        cx: &mut App,
    ) -> Task<Result<(), String>>;
    /// `chat: focus prompts`: the prompts pane of the chat shown, else the focused task's file in
    /// the center. With chats off it only says so.
    fn focus_prompts(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    );
    /// The focused task's (or project's) `prompts.txt` in the center, without the chats.
    fn open_prompts_in_center(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    );
}

/// A prompts file `prompts: use file` offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptChoice {
    /// "project: <title>" or "task: <goal>".
    pub(crate) label: String,
    pub(crate) path: PathBuf,
}

/// The [`ChatLink`] of the app: `bongos_chat` and the Work snapshot.
pub(crate) struct Chats;

impl ChatLink for Chats {
    fn panel_chat(
        &self,
        workspace: &Workspace,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<String> {
        let session = workspace.panel::<ChatPanel>(cx)?.read(cx).session()?;
        let session = session.read(cx);
        (session.prompts_editor(cx).as_ref() == Some(editor)).then(|| session.chat_id().to_string())
    }

    fn chats_for_file(&self, path: &Path, cx: &App) -> Vec<ChatRef> {
        bongos_chat::chats_for_prompts_file(path, cx)
    }

    fn auto_submit_override(&self, chat_id: &str, cx: &App) -> Option<bool> {
        ChatStore::global(cx)?
            .read(cx)
            .find(chat_id)?
            .entry
            .record()?
            .auto_submit
    }

    fn set_auto_submit(
        &self,
        chat_id: &str,
        value: Option<bool>,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let write = bongos_chat::set_auto_submit(chat_id, value, cx);
        cx.spawn(async move |_| write.await.map_err(|error| error.to_string()))
    }

    fn send(
        &self,
        chat_id: &str,
        text: String,
        options: SendOptions,
        cx: &mut App,
    ) -> Task<Result<Sent, SendError>> {
        bongos_chat::send(chat_id, text, options, cx)
    }

    fn offer_resume_and_send(
        &self,
        chat_id: String,
        name: String,
        text: String,
        options: SendOptions,
        on_sent: OnSent,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        bongos_chat::offer_resume_and_send(
            chat_id, name, text, options, on_sent, workspace, window, cx,
        )
    }

    fn task_for_dir(&self, dir: &Path, cx: &App) -> Option<String> {
        let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
        tree.task_by_dir(dir)?.id().map(str::to_string)
    }

    fn start_chat_on_task(
        &self,
        task_id: String,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        bongos_chat::start_chat_on_task(task_id, workspace, window, cx)
    }

    fn scope_dir(&self, chat_id: &str, cx: &App) -> Option<PathBuf> {
        Some(
            ChatStore::global(cx)?
                .read(cx)
                .find(chat_id)?
                .scope_dir
                .clone(),
        )
    }

    fn prompt_choices(&self, scope_dir: &Path, cx: &App) -> Vec<PromptChoice> {
        WorkStore::global(cx)
            .and_then(|store| store.read(cx).snapshot())
            .map(|tree| crate::files::choices_for(&tree, scope_dir))
            .unwrap_or_default()
    }

    fn set_prompts_file(
        &self,
        chat_id: &str,
        path: Option<PathBuf>,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let write = bongos_chat::set_prompts_file(chat_id, path, cx);
        cx.spawn(async move |_| write.await.map_err(|error| error.to_string()))
    }

    fn focus_prompts(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        bongos_chat::focus_prompts(workspace, window, cx)
    }

    fn open_prompts_in_center(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        bongos_chat::open_prompts_in_center(workspace, window, cx)
    }
}

/// The link tests put in place of [`Chats`].
struct LinkGlobal(Rc<dyn ChatLink>);

impl Global for LinkGlobal {}

/// The link in use: the one a test set, else [`Chats`].
pub(crate) fn link(cx: &App) -> Rc<dyn ChatLink> {
    match cx.try_global::<LinkGlobal>() {
        Some(global) => global.0.clone(),
        None => Rc::new(Chats),
    }
}

#[cfg(test)]
pub(crate) fn set_link(link: Rc<dyn ChatLink>, cx: &mut App) {
    cx.set_global(LinkGlobal(link));
}

/// The chat a prompts editor sends to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Attached {
    /// The chat of the panel's pane, or the one running (or picked) chat of the file.
    Chat(String),
    /// Several chats of the file run and none was picked yet.
    Several(Vec<ChatRef>),
    /// No chat of the file runs: the closed one active last.
    Closed(ChatRef),
    /// No chat uses the file.
    NoChat,
}

/// D13 on what is known: the panel's chat, the chat picked earlier, and the chats of the file in
/// the order of [`ChatLink::chats_for_file`]. A picked chat that no longer runs is forgotten.
pub(crate) fn resolve(
    panel: Option<String>,
    remembered: Option<&str>,
    chats: &[ChatRef],
) -> Attached {
    if let Some(chat) = panel {
        return Attached::Chat(chat);
    }
    let running: Vec<&ChatRef> = chats.iter().filter(|chat| chat.running).collect();
    if let Some(chat) = remembered.and_then(|id| running.iter().find(|chat| chat.id == id)) {
        return Attached::Chat(chat.id.clone());
    }
    match running.as_slice() {
        [one] => Attached::Chat(one.id.clone()),
        [] => chats
            .first()
            .map_or(Attached::NoChat, |chat| Attached::Closed(chat.clone())),
        several => Attached::Several(several.iter().map(|chat| (*chat).clone()).collect()),
    }
}

/// The path of the file `editor` shows.
pub(crate) fn file_path(editor: &Entity<Editor>, cx: &mut App) -> Option<PathBuf> {
    editor.update(cx, |editor, cx| editor.target_file_abs_path(cx))
}

/// The chat `editor` sends to (D13), from `workspace`. Nothing is attached while the editor does
/// not have the prompts mode.
pub(crate) fn attached(workspace: &Workspace, editor: &Entity<Editor>, cx: &mut App) -> Attached {
    let Some(mode) = mode_of(editor.read(cx)) else {
        return Attached::NoChat;
    };
    let link = link(cx);
    if let Some(chat) = link.panel_chat(workspace, editor, cx) {
        return Attached::Chat(chat);
    }
    let Some(path) = file_path(editor, cx) else {
        return Attached::NoChat;
    };
    let chats = link.chats_for_file(&path, cx);
    let mut mode = mode.borrow_mut();
    let attached = resolve(None, mode.chat.as_deref(), &chats);
    if !matches!(&attached, Attached::Chat(id) if Some(id) == mode.chat.as_ref()) {
        // The picked chat ended (or was never picked).
        mode.chat = None;
    }
    attached
}

/// The chat `editor` is attached to, picking among several running ones with "Send to which
/// chat?" (the choice is remembered in the mode). `then` gets the chat, or the closed one to
/// resume, or `None` when no chat uses the file. A dismissed picker calls nothing.
pub(crate) fn with_chat(
    workspace: &mut Workspace,
    editor: Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(Attached, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    match attached(workspace, &editor, cx) {
        Attached::Several(chats) => {
            let names = chats.iter().map(|chat| chat.name.clone()).collect();
            bongos_work::pick(
                workspace,
                WHICH_CHAT_QUESTION,
                names,
                0,
                Box::new(move |index, workspace, window, cx| {
                    let Some(chat) = chats.get(index) else {
                        return;
                    };
                    if let Some(mode) = mode_of(editor.read(cx)) {
                        mode.borrow_mut().chat = Some(chat.id.clone());
                    }
                    then(Attached::Chat(chat.id.clone()), workspace, window, cx);
                }),
                window,
                cx,
            );
        }
        attached => then(attached, workspace, window, cx),
    }
}

/// The effective auto-submit of `chat_id`: its override, else `prompts.auto_submit`. Without a
/// chat id it is `prompts.auto_submit`.
pub fn effective_auto_submit(chat_id: Option<&str>, cx: &App) -> bool {
    chat_id
        .and_then(|chat_id| link(cx).auto_submit_override(chat_id, cx))
        .unwrap_or(PromptsSettings::get_global(cx).auto_submit)
}

/// The override a toggle writes: the opposite of `effective`, or `None` when that is the global
/// value.
pub(crate) fn toggled(effective: bool, global: bool) -> Option<bool> {
    let new = !effective;
    (new != global).then_some(new)
}

/// The chat whose auto-submit the mode line of the file at `path` shows: the one picked, else the
/// first of the file's chats (a running one when one runs).
pub(crate) fn mode_line_chat(remembered: Option<&str>, path: &Path, cx: &App) -> Option<String> {
    let chats = link(cx).chats_for_file(path, cx);
    remembered
        .and_then(|id| chats.iter().find(|chat| chat.id == id && chat.running))
        .or(chats.first())
        .map(|chat| chat.id.clone())
}

/// `prompts: toggle auto-submit`: flips the effective auto-submit of the attached chat.
pub(crate) fn toggle_auto_submit(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = crate::commands::target(workspace, window, cx) else {
        show_toast(workspace, crate::commands::NO_VIEW_MESSAGE.to_string(), cx);
        return;
    };
    with_chat(
        workspace,
        editor,
        window,
        cx,
        |attached, workspace, _, cx| {
            let chat_id = match attached {
                Attached::Chat(id) => id,
                Attached::Closed(chat) => chat.id,
                Attached::Several(_) | Attached::NoChat => {
                    show_toast(workspace, NO_CHAT_MESSAGE.to_string(), cx);
                    return;
                }
            };
            let global = PromptsSettings::get_global(cx).auto_submit;
            let new = toggled(effective_auto_submit(Some(&chat_id), cx), global);
            let on = new.unwrap_or(global);
            let write = link(cx).set_auto_submit(&chat_id, new, cx);
            cx.spawn(async move |workspace, cx| {
                let message = match write.await {
                    Ok(()) => format!("Auto-submit {}", if on { "on" } else { "off" }),
                    Err(error) => error,
                };
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                    .ok();
            })
            .detach();
        },
    );
}
