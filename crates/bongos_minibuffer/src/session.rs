//! The native completing read: a [`Session`] is built by the host (which feature asks, what the
//! candidates are, what a result means), opened with [`open`], and answered through exactly one
//! of its `on_confirm` and `on_cancel` handlers. The minibuffer owns focus, rendering, narrowing
//! of fixed lists, history, the count and focus restore. A session whose host is not routed to
//! the minibuffer (see `minibuffer.enabled_for`) is drawn centered, as any other picker.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use bongos_minibuffer_model::Cancel;
use futures::channel::mpsc;
use gpui::{App, Context, KeyContext, SharedString, WeakEntity, Window};
use util::ResultExt as _;
use workspace::Workspace;

use crate::history_store::HistoryStore;
use crate::session_view::SessionView;

/// The value a candidate stands for, which the host downcasts to what it put there.
pub type Value = Arc<dyn Any + Send + Sync>;

/// One candidate of a session: the text that is narrowed and shown, an optional second line and
/// kind, the value the host gets back, and the input that replaces the typed text when the
/// candidate is confirmed with `descend` set (a directory, as in ivy's `ivy-alt-done`).
#[derive(Clone)]
pub struct Candidate {
    pub primary: SharedString,
    pub secondary: Option<SharedString>,
    pub kind: Option<SharedString>,
    pub value: Value,
    pub descend: Option<String>,
}

impl Candidate {
    pub fn new(primary: impl Into<SharedString>, value: impl Any + Send + Sync) -> Self {
        Self {
            primary: primary.into(),
            secondary: None,
            kind: None,
            value: Arc::new(value),
            descend: None,
        }
    }

    /// A candidate whose value is its own text, a `String`.
    pub fn text(primary: impl Into<SharedString>) -> Self {
        let primary = primary.into();
        let value = primary.to_string();
        Self::new(primary, value)
    }

    pub fn secondary(mut self, secondary: impl Into<SharedString>) -> Self {
        self.secondary = Some(secondary.into());
        self
    }

    pub fn kind(mut self, kind: impl Into<SharedString>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Confirming this candidate puts `input` in the input row instead of ending the session.
    pub fn descend(mut self, input: impl Into<String>) -> Self {
        self.descend = Some(input.into());
        self
    }
}

/// Produces the candidates for a query as they are found. It is called on every change of the
/// input; the previous call's [`Cancel`] is set first, and the batches of the previous call are
/// never shown once the input has changed. The receiver may stay open for as long as the source
/// searches.
pub type DynamicSource = Box<dyn Fn(String, Cancel, &mut App) -> mpsc::Receiver<Vec<Candidate>>>;

/// Where the candidates come from.
pub enum Source {
    /// A list known up front. The minibuffer narrows it with the model engine, off the main
    /// thread.
    Fixed(Vec<Candidate>),
    /// A source that narrows by itself, asked again for each query.
    Dynamic(DynamicSource),
}

/// What the preview handler is told.
pub enum Preview {
    /// The selected candidate changed (also for the first one shown).
    Highlight(Value),
    /// The session ended with an answer.
    Confirmed,
    /// The session ended without one.
    Cancelled,
}

/// What a confirmed session answers.
pub enum Outcome {
    /// The candidate that was selected.
    Candidate(Value),
    /// The input as typed (or the default when it is empty): only for a session with free input.
    Input(String),
}

pub(crate) type PreviewHandler = Rc<dyn Fn(Preview, &mut Window, &mut App)>;
/// Describes a candidate in a panel under the list, from its value: `None` draws no panel.
pub(crate) type DetailHandler = Rc<dyn Fn(&Value, &App) -> Option<SharedString>>;
pub(crate) type InputHandler = Rc<dyn Fn(&str, &SessionHandle, &mut Window, &mut App)>;
pub(crate) type ConfirmHandler = Box<dyn FnOnce(Outcome, &mut Window, &mut App)>;
pub(crate) type CancelHandler = Box<dyn FnOnce(&mut Window, &mut App)>;
/// Decides whether free input may be answered as it is, for a session that refuses some of it:
/// `Err` holds the refusal to show.
pub(crate) type InputCheck = Rc<dyn Fn(&str) -> Result<(), SharedString>>;
pub(crate) type ActionHandler = Rc<dyn Fn(Value, &mut Window, &mut App)>;

/// An entry of the M-o menu: it answers the session in place of `on_confirm`, with the selected
/// candidate.
pub(crate) struct SessionAction {
    pub(crate) label: SharedString,
    pub(crate) key: Option<char>,
    pub(crate) handler: ActionHandler,
}

/// A completing read, as the host describes it.
pub struct Session {
    host: &'static str,
    prompt: SharedString,
    source: Source,
    initial_input: String,
    default: Option<String>,
    free_input: bool,
    accept: Option<InputCheck>,
    history: String,
    key_context: Option<KeyContext>,
    actions: Vec<SessionAction>,
    on_preview: Option<PreviewHandler>,
    detail: Option<DetailHandler>,
    on_input: Option<InputHandler>,
    on_confirm: Option<ConfirmHandler>,
    on_cancel: Option<CancelHandler>,
}

impl Session {
    /// A completing read asked by `host`, which is the name it is routed by (listed in
    /// `minibuffer.enabled_for`) and, unless [`history`](Self::history) says otherwise, the name
    /// of its history.
    pub fn completing_read(host: &'static str, prompt: impl Into<SharedString>) -> Self {
        Self {
            host,
            prompt: prompt.into(),
            source: Source::Fixed(Vec::new()),
            initial_input: String::new(),
            default: None,
            free_input: false,
            accept: None,
            history: host.to_string(),
            key_context: None,
            actions: Vec::new(),
            on_preview: None,
            detail: None,
            on_input: None,
            on_confirm: None,
            on_cancel: None,
        }
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    pub fn initial_input(mut self, input: impl Into<String>) -> Self {
        self.initial_input = input.into();
        self
    }

    /// The candidate with this text is selected while the input is empty, and a session with free
    /// input answers it when it is confirmed with an empty input and nothing to select.
    pub fn default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    /// Whether the input can be answered as typed, when no candidate matches it (RET) or at any
    /// time (C-j).
    pub fn free_input(mut self, free_input: bool) -> Self {
        self.free_input = free_input;
        self
    }

    /// Free input is answered only when `accept` approves it (after an empty input became the
    /// default). Otherwise the session stays open and shows `refusal` after the input, until the
    /// input changes.
    pub fn accept(
        self,
        accept: impl Fn(&str) -> bool + 'static,
        refusal: impl Into<SharedString>,
    ) -> Self {
        let refusal = refusal.into();
        self.check(move |input| {
            if accept(input) {
                Ok(())
            } else {
                Err(refusal.clone())
            }
        })
    }

    /// Like [`Self::accept`], with the refusal decided by `check` for each input: `Err` holds
    /// the text shown after the input.
    pub fn check(mut self, check: impl Fn(&str) -> Result<(), SharedString> + 'static) -> Self {
        self.accept = Some(Rc::new(check));
        self
    }

    /// Names the history of this session, which is shared with every session of the same name.
    pub fn history(mut self, name: impl Into<String>) -> Self {
        self.history = name.into();
        self
    }

    /// A key context the session's view declares under `Minibuffer`, so the host can bind keys
    /// in `Minibuffer > <context> > Editor`.
    pub fn key_context(mut self, context: &str) -> Self {
        self.key_context = KeyContext::parse(context).log_err();
        self
    }

    /// An entry of the M-o menu. It runs with the selected candidate's value and ends the session
    /// in place of `on_confirm`. `key` is the key that picks it in the menu; `None` takes the
    /// first free letter of the label.
    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        key: impl Into<Option<char>>,
        handler: impl Fn(Value, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.actions.push(SessionAction {
            label: label.into(),
            key: key.into(),
            handler: Rc::new(handler),
        });
        self
    }

    pub fn on_preview(
        mut self,
        handler: impl Fn(Preview, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_preview = Some(Rc::new(handler));
        self
    }

    /// Draws the text `handler` returns for the selected candidate's value in a panel directly
    /// under the list, one line per line of text, in the buffer font. Nothing is drawn when the
    /// handler returns `None` or no candidate is selected. The text is drawn as given.
    pub fn detail(
        mut self,
        handler: impl Fn(&Value, &App) -> Option<SharedString> + 'static,
    ) -> Self {
        self.detail = Some(Rc::new(handler));
        self
    }

    /// Runs after each change of the input, with the new text.
    pub fn on_input(
        mut self,
        handler: impl Fn(&str, &SessionHandle, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_input = Some(Rc::new(handler));
        self
    }

    pub fn on_confirm(
        mut self,
        handler: impl FnOnce(Outcome, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_confirm = Some(Box::new(handler));
        self
    }

    pub fn on_cancel(mut self, handler: impl FnOnce(&mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Box::new(handler));
        self
    }
}

/// How a session ended.
pub(crate) enum Finish {
    Confirm {
        outcome: Outcome,
        /// What goes into the history, when something does.
        history: Option<String>,
    },
    Action(ActionHandler, Value),
    Cancel,
}

/// What the view and the delegate of one session share.
pub(crate) struct Core {
    pub(crate) host: &'static str,
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) view: RefCell<Option<WeakEntity<SessionView>>>,
    pub(crate) done: Cell<bool>,
    pub(crate) prompt: RefCell<SharedString>,
    pub(crate) status: RefCell<SharedString>,
    pub(crate) default: Option<String>,
    pub(crate) free_input: bool,
    pub(crate) accept: Option<InputCheck>,
    /// Whether the status shows a refusal, which the next change of the input removes.
    pub(crate) refused: Cell<bool>,
    pub(crate) history_name: String,
    pub(crate) history: HistoryStore,
    pub(crate) actions: Vec<SessionAction>,
    pub(crate) on_preview: Option<PreviewHandler>,
    pub(crate) detail: Option<DetailHandler>,
    pub(crate) on_input: Option<InputHandler>,
    on_confirm: RefCell<Option<ConfirmHandler>>,
    on_cancel: RefCell<Option<CancelHandler>>,
}

impl Core {
    pub(crate) fn handle(self: &Rc<Self>) -> SessionHandle {
        SessionHandle {
            core: self.clone(),
            view: self
                .view
                .borrow()
                .clone()
                .unwrap_or_else(WeakEntity::new_invalid),
        }
    }

    /// Ends the session, once: whatever calls this after the first time does nothing. The
    /// handlers run deferred, after the session has closed (`hide`, unless the layer is already
    /// closing it) and focus is back where it was, so a handler may open the next prompt.
    pub(crate) fn finish(
        self: &Rc<Self>,
        finish: Finish,
        hide: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.done.replace(true) {
            return;
        }
        if let Finish::Confirm {
            history: Some(entry),
            ..
        } = &finish
        {
            self.history.push(&self.history_name, entry, cx);
        }
        let this = self.clone();
        let on_confirm = self.on_confirm.take();
        let on_cancel = self.on_cancel.take();
        // Deferred, because the view is usually still being updated when it asks for this.
        window.defer(cx, move |window, cx| {
            if hide {
                this.hide_view(window, cx);
            }
            match finish {
                Finish::Confirm { outcome, .. } => {
                    this.preview(Preview::Confirmed, window, cx);
                    if let Some(on_confirm) = on_confirm {
                        on_confirm(outcome, window, cx);
                    }
                }
                Finish::Action(handler, value) => {
                    this.preview(Preview::Confirmed, window, cx);
                    handler(value, window, cx);
                }
                Finish::Cancel => {
                    this.preview(Preview::Cancelled, window, cx);
                    if let Some(on_cancel) = on_cancel {
                        on_cancel(window, cx);
                    }
                }
            }
        });
    }

    fn preview(&self, event: Preview, window: &mut Window, cx: &mut App) {
        if let Some(on_preview) = &self.on_preview {
            on_preview(event, window, cx);
        }
    }

    /// Hides the workspace's modal if it is this session's view, and only then.
    fn hide_view(&self, window: &mut Window, cx: &mut App) {
        let view = self.view.borrow().clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let active = workspace.active_modal::<SessionView>(cx);
                if active.map(|active| active.downgrade()) == view {
                    workspace.hide_modal(window, cx);
                }
            })
            .ok();
    }
}

/// What a host keeps of an open session: it changes the prompt, the status and the input, and
/// closes it. Every method does nothing once the session has ended.
#[derive(Clone)]
pub struct SessionHandle {
    core: Rc<Core>,
    view: WeakEntity<SessionView>,
}

impl SessionHandle {
    /// Replaces the prompt in front of the input ("Failing I-search: ").
    pub fn set_prompt(&self, prompt: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
        if self.core.done.get() {
            return;
        }
        *self.core.prompt.borrow_mut() = prompt.into();
        self.view
            .update(cx, |view, cx| view.refresh_prompt(window, cx))
            .ok();
    }

    /// Replaces the text after the input ("3/17"); empty text clears it.
    pub fn set_status(&self, status: impl Into<SharedString>, cx: &mut App) {
        if self.core.done.get() {
            return;
        }
        *self.core.status.borrow_mut() = status.into();
        self.view.update(cx, |_, cx| cx.notify()).ok();
    }

    /// Replaces the text of the input, which narrows the candidates as typing would.
    pub fn set_input(&self, text: &str, window: &mut Window, cx: &mut App) {
        if self.core.done.get() {
            return;
        }
        self.view
            .update(cx, |view, cx| view.set_input(text, window, cx))
            .ok();
    }

    /// Ends the session as a cancel: `on_cancel` runs, as it does for C-g.
    pub fn close(&self, window: &mut Window, cx: &mut App) {
        self.core.finish(Finish::Cancel, true, window, cx);
    }
}

/// Opens `session` as the workspace's modal, in the minibuffer when its host is routed there and
/// centered when it is not. A modal that is open is replaced: a session that was open ends as
/// cancelled first.
pub fn open(
    workspace: &mut Workspace,
    session: Session,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> SessionHandle {
    let Session {
        host,
        prompt,
        source,
        initial_input,
        default,
        free_input,
        accept,
        history,
        key_context,
        actions,
        on_preview,
        detail,
        on_input,
        on_confirm,
        on_cancel,
    } = session;
    let core = Rc::new(Core {
        host,
        workspace: cx.entity().downgrade(),
        view: RefCell::new(None),
        done: Cell::new(false),
        prompt: RefCell::new(prompt),
        status: RefCell::new(SharedString::default()),
        default,
        free_input,
        accept,
        refused: Cell::new(false),
        history: HistoryStore::global(cx),
        history_name: history,
        actions,
        on_preview,
        detail,
        on_input,
        on_confirm: RefCell::new(on_confirm),
        on_cancel: RefCell::new(on_cancel),
    });

    // `toggle_modal` alone would close a modal of the same type instead of replacing it.
    workspace.hide_modal(window, cx);
    let mut parts = Some((source, initial_input, key_context));
    workspace.toggle_modal(window, cx, |window, cx| {
        let (source, initial_input, key_context) = parts.take().expect("built once");
        SessionView::new(core.clone(), source, initial_input, key_context, window, cx)
    });
    if parts.is_some() {
        // Another modal refused to close, so this session never opened: it ends as cancelled.
        core.finish(Finish::Cancel, false, window, cx);
    }
    core.handle()
}
