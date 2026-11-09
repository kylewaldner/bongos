//! The view of a completing read: a modal that wraps a `Picker` over a [`SessionDelegate`]. The
//! delegate narrows a fixed list with the model engine on the background executor, streams a
//! dynamic source, and answers the keys of the minibuffer through the session's [`Core`].

use std::sync::{Arc, Mutex, PoisonError};

use bongos_minibuffer_model::{Cancel, Candidate as Scored, HistoryRing, Narrower, texts};
use futures::StreamExt as _;
use gpui::{
    Action, AnyElement, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    KeyContext, SharedString, Subscription, Task, prelude::*,
};
use picker::{Direction, Picker, PickerAction, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{DismissDecision, ModalView};

use crate::session::{Candidate, Core, DynamicSource, Finish, Outcome, Preview, Source, Value};

use std::rc::Rc;

/// Runs the entry `ix` of the session's M-o menu, with the selected candidate. It is dispatched
/// by the picker's actions menu and handled by the view.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = bongos_minibuffer, no_json, no_register)]
pub(crate) struct RunSessionAction {
    pub(crate) ix: usize,
}

pub struct SessionView {
    pub(crate) picker: Entity<Picker<SessionDelegate>>,
    pub(crate) core: Rc<Core>,
    key_context: Option<KeyContext>,
    _dismiss: Subscription,
}

impl SessionView {
    pub(crate) fn new(
        core: Rc<Core>,
        source: Source,
        initial_input: String,
        key_context: Option<KeyContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Before the picker exists, so that the first `on_input` already has a handle.
        core.view.replace(Some(cx.entity().downgrade()));
        let history = core.history.ring(&core.history_name);
        let delegate = SessionDelegate::new(core.clone(), source, history, &initial_input);
        let host = core.host;
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(34.))
                .minibuffer(host, cx)
        });
        if !initial_input.is_empty() {
            picker.update(cx, |picker, cx| {
                picker.set_query(&initial_input, window, cx)
            });
        }
        picker.update(cx, |picker, cx| picker.delegate.highlight(window, cx));
        let dismiss = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));
        Self {
            picker,
            core,
            key_context,
            _dismiss: dismiss,
        }
    }

    pub(crate) fn refresh_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.refresh_placeholder(window, cx));
        cx.notify();
    }

    pub(crate) fn set_input(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.set_query(text, window, cx));
    }

    fn run_action(
        &mut self,
        action: &RunSessionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.core.actions.get(action.ix) else {
            return;
        };
        let handler = entry.handler.clone();
        if let Some(value) = self.picker.read(cx).delegate.selected_value() {
            self.core
                .finish(Finish::Action(handler, value), true, window, cx);
        }
    }
}

impl EventEmitter<DismissEvent> for SessionView {}

impl Focusable for SessionView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ModalView for SessionView {
    fn minibuffer_name(&self) -> Option<&'static str> {
        Some(self.core.host)
    }

    /// Always lets the session go (a session that refused would be stranded, undrawn, when the
    /// strip goes away), and ends it as cancelled when nothing answered it yet.
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        self.core.finish(Finish::Cancel, false, window, cx);
        DismissDecision::Dismiss(true)
    }
}

impl Render for SessionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.core.status.borrow().clone();
        let status =
            (!status.is_empty()).then(|| Label::new(status).color(Color::Muted).single_line());
        let routed = workspace::routes_to_minibuffer(self.core.host, cx);
        let body: AnyElement = if routed {
            h_flex()
                .w_full()
                .items_center()
                .child(div().flex_1().min_w_0().child(self.picker.clone()))
                .children(status.map(|status| div().flex_none().px_2().child(status)))
                .into_any_element()
        } else {
            v_flex()
                .child(self.picker.clone())
                .children(status.map(|status| div().px_2().child(status)))
                .into_any_element()
        };
        div()
            .id("minibuffer-session")
            .when_some(self.key_context.clone(), |this, context| {
                this.key_context(context)
            })
            .on_action(cx.listener(Self::run_action))
            .child(body)
    }
}

/// The candidates that are shown, in order.
enum Shown {
    /// Every candidate, in source order.
    All,
    /// The matches of the last narrowing pass, best first.
    Matches(Arc<[Scored]>),
}

enum Kind {
    Fixed(Arc<Mutex<Narrower>>),
    Dynamic(Rc<DynamicSource>),
}

pub struct SessionDelegate {
    core: Rc<Core>,
    kind: Kind,
    items: Vec<Candidate>,
    shown: Shown,
    selected: usize,
    /// The input as the last pass saw it.
    query: String,
    /// The input the host was last told about.
    last_input: String,
    /// Which pass the delegate is waiting for; a result of any other is dropped.
    generation: usize,
    /// Stops the pass of the previous query, which a new one makes stale.
    cancel: Cancel,
    last_highlight: Option<Value>,
    history: HistoryRing,
    history_position: Option<usize>,
    /// The input that was typed before the history was walked, and the entry shown from it.
    history_saved: String,
    history_shown: Option<String>,
}

impl SessionDelegate {
    fn new(core: Rc<Core>, source: Source, history: HistoryRing, initial_input: &str) -> Self {
        let (kind, items) = match source {
            Source::Fixed(items) => {
                let narrower = Narrower::new(texts(items.iter().map(|item| item.primary.as_ref())));
                (Kind::Fixed(Arc::new(Mutex::new(narrower))), items)
            }
            Source::Dynamic(source) => (Kind::Dynamic(Rc::new(source)), Vec::new()),
        };
        let mut this = Self {
            core,
            kind,
            items,
            shown: Shown::All,
            selected: 0,
            query: String::new(),
            last_input: initial_input.to_string(),
            generation: 0,
            cancel: Cancel::new(),
            last_highlight: None,
            history,
            history_position: None,
            history_saved: String::new(),
            history_shown: None,
        };
        this.select_initial();
        this
    }

    fn candidate(&self, ix: usize) -> Option<&Candidate> {
        match &self.shown {
            Shown::All => self.items.get(ix),
            Shown::Matches(matches) => self.items.get(matches.get(ix)?.index as usize),
        }
    }

    fn len(&self) -> usize {
        match &self.shown {
            Shown::All => self.items.len(),
            Shown::Matches(matches) => matches.len(),
        }
    }

    /// The text of every candidate that is shown, in order.
    #[cfg(test)]
    pub(crate) fn shown_texts(&self) -> Vec<String> {
        (0..self.len())
            .filter_map(|ix| self.candidate(ix))
            .map(|candidate| candidate.primary.to_string())
            .collect()
    }

    pub(crate) fn selected_value(&self) -> Option<Value> {
        self.candidate(self.selected)
            .map(|candidate| candidate.value.clone())
    }

    /// The default's row while the input is empty, else the first.
    fn select_initial(&mut self) {
        self.selected = match (&self.core.default, self.query.trim().is_empty()) {
            (Some(default), true) => (0..self.len())
                .find(|ix| {
                    self.candidate(*ix)
                        .is_some_and(|candidate| candidate.primary.as_ref() == default)
                })
                .unwrap_or(0),
            _ => 0,
        };
    }

    /// Tells the preview handler which candidate is selected, when that changed.
    pub(crate) fn highlight(&mut self, window: &mut Window, cx: &mut App) {
        let Some(on_preview) = self.core.on_preview.clone() else {
            return;
        };
        let value = self.selected_value();
        let same = match (&value, &self.last_highlight) {
            (Some(new), Some(old)) => std::ptr::addr_eq(Arc::as_ptr(new), Arc::as_ptr(old)),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.last_highlight = value.clone();
        let Some(value) = value else {
            return;
        };
        let core = self.core.clone();
        window.defer(cx, move |window, cx| {
            if !core.done.get() {
                on_preview(Preview::Highlight(value), window, cx);
            }
        });
    }

    /// Tells the host about a changed input, after the current update.
    fn notify_input(&mut self, query: &str, window: &mut Window, cx: &mut App) {
        if self.last_input == query {
            return;
        }
        self.last_input = query.to_string();
        let Some(on_input) = self.core.on_input.clone() else {
            return;
        };
        let core = self.core.clone();
        let query = query.to_string();
        window.defer(cx, move |window, cx| {
            if !core.done.get() {
                on_input(&query, &core.handle(), window, cx);
            }
        });
    }

    fn input_outcome(&self) -> Outcome {
        let input = if self.query.is_empty() {
            self.core.default.clone().unwrap_or_default()
        } else {
            self.query.clone()
        };
        Outcome::Input(input)
    }
}

impl PickerDelegate for SessionDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "minibuffer session"
    }

    fn match_count(&self) -> usize {
        self.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected = ix;
        self.highlight(window, cx);
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.core.prompt.borrow().to_string().into()
    }

    fn prompt_label(&self, _: &App) -> Option<SharedString> {
        Some(self.core.prompt.borrow().clone())
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        // With free input, text that matches nothing is a normal thing to type.
        (!self.core.free_input).then(|| "No matches".into())
    }

    fn match_text(&self, ix: usize) -> Option<String> {
        self.candidate(ix)
            .map(|candidate| candidate.primary.to_string())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        // A new input makes the pass of the previous one stale, whatever it has found.
        self.cancel.cancel();
        self.cancel = Cancel::new();
        self.generation += 1;
        let generation = self.generation;
        if self.history_shown.as_deref() != Some(query.as_str()) {
            self.history_position = None;
            self.history_shown = None;
        }
        self.notify_input(&query, window, cx);
        self.query = query.clone();

        match &self.kind {
            Kind::Fixed(narrower) => {
                if query.trim().is_empty() {
                    self.shown = Shown::All;
                    self.select_initial();
                    self.highlight(window, cx);
                    return Task::ready(());
                }
                let narrower = narrower.clone();
                let cancel = self.cancel.clone();
                let pass = cx.background_spawn(async move {
                    narrower
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .narrow(&query, &cancel)
                });
                cx.spawn_in(window, async move |picker, cx| {
                    let Some(matches) = pass.await else {
                        return;
                    };
                    picker
                        .update_in(cx, |picker, window, cx| {
                            if picker.delegate.generation != generation {
                                return;
                            }
                            picker.delegate.shown = Shown::Matches(matches);
                            picker.delegate.selected = 0;
                            picker.delegate.highlight(window, cx);
                            cx.notify();
                        })
                        .ok();
                })
            }
            Kind::Dynamic(source) => {
                let mut batches = source(query, self.cancel.clone(), cx);
                self.items.clear();
                self.shown = Shown::All;
                self.selected = 0;
                self.last_highlight = None;
                cx.spawn_in(window, async move |picker, cx| {
                    while let Some(batch) = batches.next().await {
                        let current = picker.update_in(cx, |picker, window, cx| {
                            if picker.delegate.generation != generation {
                                return false;
                            }
                            picker.delegate.items.extend(batch);
                            picker.delegate.highlight(window, cx);
                            cx.notify();
                            true
                        });
                        if !current.unwrap_or(false) {
                            break;
                        }
                    }
                })
            }
        }
    }

    /// A streaming source is never waited for: confirming acts on what is shown.
    fn finalize_update_matches(
        &mut self,
        _: String,
        _: std::time::Duration,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> bool {
        matches!(self.kind, Kind::Dynamic(_))
    }

    fn select_history(
        &mut self,
        direction: Direction,
        query: &str,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        let next = match (direction, self.history_position) {
            (Direction::Up, None) => {
                self.history_saved = query.to_string();
                Some(0)
            }
            (Direction::Up, Some(position)) => Some(position + 1),
            (Direction::Down, Some(0)) => None,
            (Direction::Down, Some(position)) => Some(position - 1),
            (Direction::Down, None) => return None,
        };
        let entry = match next {
            Some(position) => {
                let entry = self.history.get(position)?.to_string();
                self.history_position = Some(position);
                self.history_shown = Some(entry.clone());
                entry
            }
            None => {
                self.history_position = None;
                self.history_shown = None;
                std::mem::take(&mut self.history_saved)
            }
        };
        Some(entry)
    }

    fn confirm_update_query(
        &mut self,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        self.candidate(self.selected)?.descend.clone()
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let finish = match self.candidate(self.selected) {
            Some(candidate) => Finish::Confirm {
                outcome: Outcome::Candidate(candidate.value.clone()),
                history: Some(if self.query.is_empty() {
                    candidate.primary.to_string()
                } else {
                    self.query.clone()
                }),
            },
            None if self.core.free_input => self.input_finish(),
            None => return,
        };
        self.core.finish(finish, true, window, cx);
    }

    fn confirm_input(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.core.free_input {
            let finish = self.input_finish();
            self.core.finish(finish, true, window, cx);
        }
    }

    fn actions_menu(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Vec<PickerAction> {
        self.core
            .actions
            .iter()
            .enumerate()
            .map(|(ix, action)| {
                let entry = PickerAction::button(
                    action.label.clone(),
                    RunSessionAction { ix }.boxed_clone(),
                );
                match action.key {
                    Some(key) => entry.key(key),
                    None => entry,
                }
            })
            .collect()
    }

    /// The view ends the session when the picker says it was dismissed.
    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.candidate(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(candidate.primary.clone()))
                        .children(candidate.secondary.clone().map(|secondary| {
                            Label::new(secondary)
                                .color(Color::Muted)
                                .size(LabelSize::Small)
                        }))
                        .children(candidate.kind.clone().map(|kind| {
                            Label::new(kind).color(Color::Muted).size(LabelSize::Small)
                        })),
                ),
        )
    }
}

impl SessionDelegate {
    fn input_finish(&self) -> Finish {
        let outcome = self.input_outcome();
        let history = match &outcome {
            Outcome::Input(input) if !input.is_empty() => Some(input.clone()),
            _ => None,
        };
        Finish::Confirm { outcome, history }
    }
}
