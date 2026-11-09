//! Tests of the attached chat (D13) and auto-submit (D15): the resolution rules, the panel's
//! pane, one running chat, the picker among several (remembered), only closed chats (resume
//! offer), no chat (start offer, or a toast when no task owns the file), the toggle's
//! `Some`/`None` writes, and the mode line. The chats crate is replaced by [`FakeChats`].

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use bongos_chat::{ChatRef, ChatsSettings, SendError, SendOptions, Sent};
use collections::HashMap;
use editor::{Editor, SelectionEffects};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, BorrowAppContext as _, Context, Entity, Focusable as _, Task,
    TestAppContext, VisualTestContext, Window,
};
use language::Point;
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;

use crate::actions::TOASTS;
use crate::attach::{
    Attached, ChatLink, NO_CHAT_MESSAGE, OnSent, PromptChoice, resolve, set_link, toggled,
};
use crate::mode_tests::{dispatch, open_in, workspace_on};
use crate::send::NO_TASK_MESSAGE;

/// The prompts file every test opens (as `mode_tests` writes it). Blocks: 0 the preamble (row 0),
/// 1 sent (row 2), 2 pending (rows 4 and 5), 3 pending (row 7).
pub(crate) const THREE: &str = "\
preamble
=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===
first, sent
=== prompt 2026-11-05 14:10 ===
second
  more
=== prompt 2026-11-05 14:30 ===
third";

pub(crate) const PATH: &str = "/dir/prompts.txt";

/// A resume offer the fake was asked to make.
pub(crate) struct Offer {
    pub(crate) chat_id: String,
    pub(crate) name: String,
    pub(crate) text: String,
    pub(crate) options: SendOptions,
    pub(crate) on_sent: Option<OnSent>,
}

/// The chats crate as the tests want it: chats of the file, overrides, recorded sends and
/// offers, and sends that answer at once or wait for the test.
pub(crate) struct FakeChats {
    pub(crate) panel: RefCell<Option<String>>,
    pub(crate) chats: RefCell<Vec<ChatRef>>,
    pub(crate) overrides: RefCell<HashMap<String, bool>>,
    pub(crate) writes: RefCell<Vec<(String, Option<bool>)>>,
    pub(crate) sends: RefCell<Vec<(String, String, SendOptions)>>,
    pub(crate) reply: RefCell<Result<Sent, SendError>>,
    pub(crate) hold: Cell<bool>,
    pub(crate) held: RefCell<Vec<oneshot::Sender<Result<Sent, SendError>>>>,
    pub(crate) offers: RefCell<Vec<Offer>>,
    pub(crate) task: RefCell<Option<String>>,
    pub(crate) started: RefCell<Vec<String>>,
    pub(crate) scope: RefCell<Option<PathBuf>>,
    pub(crate) choices: RefCell<Vec<PromptChoice>>,
    /// The writes of the prompts file, and the record they leave: `None` is the default file.
    pub(crate) file_writes: RefCell<Vec<(String, Option<PathBuf>)>>,
    pub(crate) record_file: RefCell<Option<PathBuf>>,
    pub(crate) focus_calls: Cell<usize>,
    pub(crate) center_calls: Cell<usize>,
}

pub(crate) fn sent() -> Sent {
    Sent {
        submitted: true,
        named: None,
    }
}

impl Default for FakeChats {
    fn default() -> Self {
        Self {
            panel: RefCell::default(),
            chats: RefCell::default(),
            overrides: RefCell::default(),
            writes: RefCell::default(),
            sends: RefCell::default(),
            reply: RefCell::new(Ok(sent())),
            hold: Cell::new(false),
            held: RefCell::default(),
            offers: RefCell::default(),
            task: RefCell::default(),
            started: RefCell::default(),
            scope: RefCell::default(),
            choices: RefCell::default(),
            file_writes: RefCell::default(),
            record_file: RefCell::default(),
            focus_calls: Cell::new(0),
            center_calls: Cell::new(0),
        }
    }
}

impl ChatLink for FakeChats {
    fn panel_chat(&self, _: &Workspace, _: &Entity<Editor>, _: &App) -> Option<String> {
        self.panel.borrow().clone()
    }

    fn chats_for_file(&self, path: &Path, _: &App) -> Vec<ChatRef> {
        if path == Path::new(PATH) {
            self.chats.borrow().clone()
        } else {
            Vec::new()
        }
    }

    fn auto_submit_override(&self, chat_id: &str, _: &App) -> Option<bool> {
        self.overrides.borrow().get(chat_id).copied()
    }

    fn set_auto_submit(
        &self,
        chat_id: &str,
        value: Option<bool>,
        _: &mut App,
    ) -> Task<Result<(), String>> {
        self.writes.borrow_mut().push((chat_id.to_string(), value));
        match value {
            Some(value) => self
                .overrides
                .borrow_mut()
                .insert(chat_id.to_string(), value),
            None => self.overrides.borrow_mut().remove(chat_id),
        };
        Task::ready(Ok(()))
    }

    fn send(
        &self,
        chat_id: &str,
        text: String,
        options: SendOptions,
        cx: &mut App,
    ) -> Task<Result<Sent, SendError>> {
        self.sends
            .borrow_mut()
            .push((chat_id.to_string(), text, options));
        if self.hold.get() {
            let (sender, receiver) = oneshot::channel();
            self.held.borrow_mut().push(sender);
            return cx
                .background_spawn(async move { receiver.await.unwrap_or(Err(SendError::Off)) });
        }
        Task::ready(self.reply.borrow().clone())
    }

    fn offer_resume_and_send(
        &self,
        chat_id: String,
        name: String,
        text: String,
        options: SendOptions,
        on_sent: OnSent,
        _: &mut Workspace,
        _: &mut Window,
        _: &mut Context<Workspace>,
    ) {
        self.offers.borrow_mut().push(Offer {
            chat_id,
            name,
            text,
            options,
            on_sent: Some(on_sent),
        });
    }

    fn task_for_dir(&self, dir: &Path, _: &App) -> Option<String> {
        (dir == Path::new("/dir"))
            .then(|| self.task.borrow().clone())
            .flatten()
    }

    fn start_chat_on_task(
        &self,
        task_id: String,
        _: &mut Workspace,
        _: &mut Window,
        _: &mut Context<Workspace>,
    ) {
        self.started.borrow_mut().push(task_id);
    }

    fn scope_dir(&self, _: &str, _: &App) -> Option<PathBuf> {
        self.scope.borrow().clone()
    }

    fn prompt_choices(&self, _: &Path, _: &App) -> Vec<PromptChoice> {
        self.choices.borrow().clone()
    }

    fn set_prompts_file(
        &self,
        chat_id: &str,
        path: Option<PathBuf>,
        _: &mut App,
    ) -> Task<Result<(), String>> {
        self.file_writes
            .borrow_mut()
            .push((chat_id.to_string(), path.clone()));
        self.record_file.replace(path);
        Task::ready(Ok(()))
    }

    fn focus_prompts(&self, _: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {
        self.focus_calls.set(self.focus_calls.get() + 1);
    }

    fn open_prompts_in_center(
        &self,
        _: &mut Workspace,
        _: &mut Window,
        _: &mut Context<Workspace>,
    ) {
        self.center_calls.set(self.center_calls.get() + 1);
    }
}

pub(crate) fn chat(id: &str, running: bool, last_active: &str) -> ChatRef {
    ChatRef {
        id: id.to_string(),
        name: format!("chat {id}"),
        running,
        last_active: last_active.parse().unwrap(),
    }
}

pub(crate) fn set_chats_enabled(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.chats.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

pub(crate) fn update_prompts_settings(
    cx: &mut VisualTestContext,
    change: impl FnOnce(&mut settings::PromptsSettingsContent),
) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                change(content.prompts.get_or_insert_default())
            });
        });
    });
}

/// A workspace with `prompts.txt` (`THREE`) open and focused, point at `row`, chats on, and the
/// fake in place of the chats crate.
pub(crate) async fn setup(
    cx: &mut TestAppContext,
    enabled: bool,
    row: u32,
) -> (
    Entity<Workspace>,
    Entity<Editor>,
    Rc<FakeChats>,
    &mut VisualTestContext,
) {
    TOASTS.with(|toasts| toasts.borrow_mut().clear());
    let (workspace, cx) = workspace_on(cx, enabled).await;
    let fake = Rc::new(FakeChats::default());
    cx.update(|_, cx| {
        ChatsSettings::register(cx);
        set_link(fake.clone(), cx);
    });
    set_chats_enabled(cx, true);
    let editor = open_in(&workspace, "prompts.txt", cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();
    put(&editor, Point::new(row, 0), cx);
    assert_eq!(text(&editor, cx), THREE);
    (workspace, editor, fake, cx)
}

pub(crate) fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| editor.text(cx))
}

pub(crate) fn select(editor: &Entity<Editor>, from: Point, to: Point, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([from..to])
        })
    });
    cx.run_until_parked();
}

pub(crate) fn put(editor: &Entity<Editor>, at: Point, cx: &mut VisualTestContext) {
    select(editor, at, at, cx);
}

pub(crate) fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

/// The toasts shown since the last call.
pub(crate) fn toasts() -> Vec<String> {
    TOASTS.with(|toasts| std::mem::take(&mut *toasts.borrow_mut()))
}

pub(crate) fn has_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

/// Waits for a picker, types `text` into it and confirms.
pub(crate) async fn answer(workspace: &Entity<Workspace>, text: &str, cx: &mut VisualTestContext) {
    for _ in 0..100 {
        cx.run_until_parked();
        if has_modal(workspace, cx) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    assert!(has_modal(workspace, cx), "no picker came for `{text}`");
    cx.simulate_input(text);
    cx.run_until_parked();
    dispatch("menu::Confirm", cx);
    cx.run_until_parked();
}

pub(crate) fn dismiss_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
    cx.run_until_parked();
}

const T1: &str = "2026-11-05T10:00:00Z";
const T2: &str = "2026-11-05T11:00:00Z";

#[test]
fn the_panel_chat_wins() {
    let chats = [chat("a", true, T1)];
    assert_eq!(
        resolve(Some("p".into()), None, &chats),
        Attached::Chat("p".into())
    );
}

#[test]
fn one_running_chat_is_attached() {
    let chats = [chat("a", true, T1), chat("b", false, T2)];
    assert_eq!(resolve(None, None, &chats), Attached::Chat("a".into()));
}

#[test]
fn several_running_chats_ask_unless_one_was_picked() {
    let chats = [
        chat("a", true, T2),
        chat("b", true, T1),
        chat("c", false, T1),
    ];
    assert_eq!(
        resolve(None, None, &chats),
        Attached::Several(vec![chat("a", true, T2), chat("b", true, T1)])
    );
    assert_eq!(resolve(None, Some("b"), &chats), Attached::Chat("b".into()));
    // A picked chat that ended is forgotten.
    assert!(matches!(
        resolve(None, Some("c"), &chats),
        Attached::Several(_)
    ));
}

#[test]
fn only_closed_chats_give_the_one_active_last() {
    // `chats_for_file` lists the one active last first.
    let chats = [chat("b", false, T2), chat("a", false, T1)];
    assert_eq!(
        resolve(None, None, &chats),
        Attached::Closed(chat("b", false, T2))
    );
    assert_eq!(resolve(None, None, &[]), Attached::NoChat);
}

#[test]
fn a_toggle_writes_the_opposite_or_none_when_that_is_the_global_value() {
    assert_eq!(toggled(true, true), Some(false));
    assert_eq!(toggled(false, true), None);
    assert_eq!(toggled(false, false), Some(true));
    assert_eq!(toggled(true, false), None);
}

fn mode_line(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.read_with(cx, |editor, cx| {
        crate::mode_line(editor, cx).unwrap().to_string()
    })
}

#[gpui::test]
async fn toggle_auto_submit_writes_some_then_none(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats.replace(vec![chat("a", true, T1)]);
    assert!(mode_line(&editor, cx).ends_with("auto-submit on"));

    dispatch("prompts::ToggleAutoSubmit", cx);
    assert_eq!(*fake.writes.borrow(), vec![("a".to_string(), Some(false))]);
    assert_eq!(toasts(), vec!["Auto-submit off".to_string()]);
    assert!(mode_line(&editor, cx).ends_with("auto-submit off"));

    dispatch("prompts::ToggleAutoSubmit", cx);
    assert_eq!(fake.writes.borrow()[1], ("a".to_string(), None));
    assert!(mode_line(&editor, cx).ends_with("auto-submit on"));

    // With the global value off, turning it on is an override.
    update_prompts_settings(cx, |prompts| prompts.auto_submit = Some(false));
    assert!(mode_line(&editor, cx).ends_with("auto-submit off"));
    dispatch("prompts::ToggleAutoSubmit", cx);
    assert_eq!(fake.writes.borrow()[2], ("a".to_string(), Some(true)));
    assert!(mode_line(&editor, cx).ends_with("auto-submit on"));
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn toggle_auto_submit_without_a_chat_says_so(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = setup(cx, true, 4).await;
    dispatch("prompts::ToggleAutoSubmit", cx);
    assert!(fake.writes.borrow().is_empty());
    assert_eq!(toasts(), vec![NO_CHAT_MESSAGE.to_string()]);
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn the_panel_pane_sends_to_the_panel_chat(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats.replace(vec![chat("a", true, T1)]);
    fake.panel.replace(Some("p".into()));
    dispatch("prompts::SendWithoutMarking", cx);
    assert_eq!(fake.sends.borrow()[0].0, "p");
}

#[gpui::test]
async fn one_running_chat_gets_the_send_with_its_auto_submit(cx: &mut TestAppContext) {
    let (_workspace, _editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats
        .replace(vec![chat("a", true, T1), chat("b", false, T2)]);
    fake.overrides.borrow_mut().insert("a".into(), false);
    dispatch("prompts::SendWithoutMarking", cx);
    let sends = fake.sends.borrow();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, "a");
    assert_eq!(sends[0].1, "second\n  more");
    assert_eq!(sends[0].2, SendOptions { submit: false });
}

#[gpui::test]
async fn several_running_chats_ask_once_and_remember(cx: &mut TestAppContext) {
    let (workspace, _editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats
        .replace(vec![chat("a", true, T2), chat("b", true, T1)]);
    dispatch("prompts::SendWithoutMarking", cx);
    assert!(
        fake.sends.borrow().is_empty(),
        "nothing is sent before the pick"
    );
    assert!(has_modal(&workspace, cx));
    answer(&workspace, "chat b", cx).await;
    assert_eq!(fake.sends.borrow().len(), 1);
    assert_eq!(fake.sends.borrow()[0].0, "b");

    // The choice is remembered for this editor.
    dispatch("prompts::SendWithoutMarking", cx);
    assert!(!has_modal(&workspace, cx));
    assert_eq!(fake.sends.borrow()[1].0, "b");

    // Until that chat ends.
    fake.chats.replace(vec![
        chat("a", true, T2),
        chat("b", false, T1),
        chat("c", true, T1),
    ]);
    dispatch("prompts::SendWithoutMarking", cx);
    assert!(has_modal(&workspace, cx));
    dismiss_modal(&workspace, cx);
    assert_eq!(fake.sends.borrow().len(), 2);
}

#[gpui::test]
async fn only_closed_chats_offer_a_resume(cx: &mut TestAppContext) {
    let (_workspace, editor, fake, cx) = setup(cx, true, 4).await;
    fake.chats
        .replace(vec![chat("b", false, T2), chat("a", false, T1)]);
    dispatch("prompts::Send", cx);
    assert!(fake.sends.borrow().is_empty());
    let offers = fake.offers.borrow();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].chat_id, "b");
    assert_eq!(offers[0].name, "chat b");
    assert_eq!(offers[0].text, "second\n  more");
    assert_eq!(offers[0].options, SendOptions { submit: true });
    drop(offers);
    // Until the offer sends, nothing is marked.
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn no_chat_offers_to_start_one_on_the_task(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = setup(cx, true, 4).await;
    fake.task.replace(Some("t1".into()));
    dispatch("prompts::Send", cx);
    assert!(has_modal(&workspace, cx));
    answer(&workspace, "Start", cx).await;
    assert_eq!(*fake.started.borrow(), vec!["t1".to_string()]);
    assert!(fake.sends.borrow().is_empty());
    assert_eq!(text(&editor, cx), THREE);

    // Cancel starts nothing.
    dispatch("prompts::Send", cx);
    answer(&workspace, "Cancel", cx).await;
    assert_eq!(fake.started.borrow().len(), 1);
    assert_eq!(text(&editor, cx), THREE);
}

#[gpui::test]
async fn no_chat_and_no_task_says_so(cx: &mut TestAppContext) {
    let (workspace, editor, fake, cx) = setup(cx, true, 4).await;
    dispatch("prompts::Send", cx);
    assert!(!has_modal(&workspace, cx));
    assert_eq!(toasts(), vec![NO_TASK_MESSAGE.to_string()]);
    assert!(fake.sends.borrow().is_empty());
    assert!(fake.started.borrow().is_empty());
    assert_eq!(text(&editor, cx), THREE);
}
