//! The two modals the commands chain: a list to pick from and a line of text to type. Each calls
//! its continuation after it has closed itself, so a continuation may open the next modal of
//! either kind. When `work` is listed in `minibuffer.enabled_for`, both open in the minibuffer
//! strip as a completing read and a string read instead, with the same continuation.

use std::rc::Rc;
use std::sync::Arc;

use bongos_minibuffer::{Candidate, Outcome, Session, Source};
use editor::{Editor, EditorEvent};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

/// What runs once a choice or a line is made, on the workspace and after the modal has closed.
pub type Then<T> =
    Box<dyn FnOnce(T, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static>;

/// The name the work prompts are routed by (`minibuffer.enabled_for`).
pub const MINIBUFFER_NAME: &str = "work";

/// Runs `then` on the workspace once a minibuffer session has answered (and closed).
fn run_then<T>(
    workspace: &WeakEntity<Workspace>,
    value: T,
    then: Then<T>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| then(value, workspace, window, cx))
        .ok();
}

/// Closes the active modal, then runs `then`.
fn finish<T: 'static>(
    workspace: &WeakEntity<Workspace>,
    value: T,
    then: Then<T>,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = workspace.clone();
    // Deferred, because the modal is still being updated when it asks for this.
    window.defer(cx, move |window, cx| {
        workspace
            .update(cx, |workspace, cx| {
                workspace.hide_modal(window, cx);
                then(value, workspace, window, cx);
            })
            .ok();
    });
}

/// Shows `build` as the modal, replacing any other, including one of the same type (which
/// `toggle_modal` alone would close).
pub(crate) fn show<V: ModalView>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) {
    workspace.hide_modal(window, cx);
    workspace.toggle_modal(window, cx, build);
}

/// Opens a list of `choices`; `then` gets the index of the one confirmed. `selected` is the row
/// highlighted first.
pub fn pick(
    workspace: &mut Workspace,
    placeholder: impl Into<SharedString>,
    choices: Vec<String>,
    selected: usize,
    then: Then<usize>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let placeholder = placeholder.into();
    if workspace::routes_to_minibuffer(MINIBUFFER_NAME, cx) {
        let candidates = choices
            .iter()
            .enumerate()
            .map(|(index, label)| Candidate::new(label.clone(), index))
            .collect();
        let mut session = Session::completing_read(MINIBUFFER_NAME, format!("{placeholder}: "))
            .source(Source::Fixed(candidates))
            .history(format!("{MINIBUFFER_NAME} {placeholder}"))
            .on_confirm(move |outcome, window, cx| {
                if let Outcome::Candidate(value) = outcome
                    && let Some(&index) = value.downcast_ref::<usize>()
                {
                    run_then(&weak, index, then, window, cx);
                }
            });
        if selected > 0
            && let Some(label) = choices.get(selected)
        {
            session = session.default(label.clone());
        }
        bongos_minibuffer::open(workspace, session, window, cx);
        return;
    }
    show(workspace, window, cx, |window, cx| {
        let delegate = ChoiceDelegate {
            matches: (0..choices.len()).collect(),
            selected_index: selected.min(choices.len().saturating_sub(1)),
            choices,
            placeholder,
            picker: cx.entity().downgrade(),
            workspace: weak,
            then: Some(then),
        };
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(34.)));
        ChoicePicker { picker }
    });
}

pub(crate) struct ChoicePicker {
    pub(crate) picker: Entity<Picker<ChoiceDelegate>>,
}

impl ModalView for ChoicePicker {}

impl EventEmitter<DismissEvent> for ChoicePicker {}

impl Focusable for ChoicePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ChoicePicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

pub(crate) struct ChoiceDelegate {
    pub(crate) choices: Vec<String>,
    /// Indexes into `choices` that match the query.
    matches: Vec<usize>,
    selected_index: usize,
    placeholder: SharedString,
    picker: WeakEntity<ChoicePicker>,
    workspace: WeakEntity<Workspace>,
    then: Option<Then<usize>>,
}

impl PickerDelegate for ChoiceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "work choice"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.to_string().into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let kept_before = self.matches.get(self.selected_index).copied();
        self.matches = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, label)| {
                let label = label.to_lowercase();
                words.iter().all(|word| label.contains(word))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = kept_before
            .and_then(|kept| self.matches.iter().position(|&index| index == kept))
            .unwrap_or(0);
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(&index) = self.matches.get(self.selected_index) else {
            return;
        };
        if let Some(then) = self.then.take() {
            finish(&self.workspace, index, then, window, cx);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let label = self.choices.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    SharedString::from(label.clone()),
                    Vec::new(),
                )),
        )
    }
}

/// Opens a line of text titled `title`, starting with `initial`; `then` gets the trimmed text
/// when it is confirmed. An empty line is refused unless `allow_empty`.
pub fn ask(
    workspace: &mut Workspace,
    title: impl Into<SharedString>,
    initial: &str,
    allow_empty: bool,
    then: Then<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let title = title.into();
    if workspace::routes_to_minibuffer(MINIBUFFER_NAME, cx) {
        let session = Session::string(MINIBUFFER_NAME, format!("{title}: "))
            .initial_input(initial)
            .history(format!("{MINIBUFFER_NAME} {title}"))
            .accept(
                move |text| allow_empty || !text.trim().is_empty(),
                "Type something, or press C-g to cancel",
            )
            .on_confirm(move |outcome, window, cx| {
                if let Outcome::Input(text) = outcome {
                    run_then(&weak, text.trim().to_string(), then, window, cx);
                }
            });
        bongos_minibuffer::open(workspace, session, window, cx);
        return;
    }
    show(workspace, window, cx, |window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        TextPrompt {
            title,
            editor,
            allow_empty,
            hint: None,
            hint_is_error: false,
            check: None,
            workspace: weak,
            then: Some(then),
            _edits: None,
        }
    });
}

/// Checks the trimmed text of [`ask_checked`]: `Ok` holds the status to show, `Err` the error
/// that keeps the text from being confirmed.
pub type Check = Rc<dyn Fn(&str) -> Result<String, String>>;

/// The status a [`Check`] shows for `text`: its error, or what confirming would do.
fn checked_status(check: &Check, text: &str) -> String {
    match check(text.trim()) {
        Ok(status) | Err(status) => status,
    }
}

/// Like [`ask`], with `check` run on every edit: the status under the line (after it, in the
/// minibuffer) shows its error or its message, and text it refuses cannot be confirmed. Empty
/// text is passed to `check` like any other.
pub fn ask_checked(
    workspace: &mut Workspace,
    title: impl Into<SharedString>,
    initial: &str,
    check: Check,
    then: Then<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let title = title.into();
    if workspace::routes_to_minibuffer(MINIBUFFER_NAME, cx) {
        let on_input = check.clone();
        let refuse = check.clone();
        let session = Session::string(MINIBUFFER_NAME, format!("{title}: "))
            .initial_input(initial)
            .history(format!("{MINIBUFFER_NAME} {title}"))
            .check(move |text| refuse(text.trim()).map(|_| ()).map_err(Into::into))
            .on_input(move |text, handle, _, cx| {
                handle.set_status(checked_status(&on_input, text), cx);
            })
            .on_confirm(move |outcome, window, cx| {
                if let Outcome::Input(text) = outcome {
                    run_then(&weak, text.trim().to_string(), then, window, cx);
                }
            });
        let handle = bongos_minibuffer::open(workspace, session, window, cx);
        handle.set_status(checked_status(&check, initial), cx);
        return;
    }
    show(workspace, window, cx, |window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        let edits = cx.subscribe(&editor, |prompt: &mut TextPrompt, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                prompt.refresh_check(cx);
            }
        });
        let mut prompt = TextPrompt {
            title,
            editor,
            allow_empty: true,
            hint: None,
            hint_is_error: false,
            check: Some(check),
            workspace: weak,
            then: Some(then),
            _edits: Some(edits),
        };
        prompt.refresh_check(cx);
        prompt
    });
}

pub(crate) struct TextPrompt {
    title: SharedString,
    pub(crate) editor: Entity<Editor>,
    allow_empty: bool,
    /// Shown in place of the title: why the text was refused, or the status of a check.
    pub(crate) hint: Option<SharedString>,
    pub(crate) hint_is_error: bool,
    check: Option<Check>,
    workspace: WeakEntity<Workspace>,
    then: Option<Then<String>>,
    _edits: Option<Subscription>,
}

impl ModalView for TextPrompt {}

impl EventEmitter<DismissEvent> for TextPrompt {}

impl Focusable for TextPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl TextPrompt {
    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Shows what the check says about the text as it is now.
    fn refresh_check(&mut self, cx: &mut Context<Self>) {
        let Some(check) = &self.check else {
            return;
        };
        let text = self.editor.read(cx).text(cx);
        let (hint, is_error) = match check(text.trim()) {
            Ok(status) => (status, false),
            Err(error) => (error, true),
        };
        self.hint = Some(hint.into());
        self.hint_is_error = is_error;
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() && !self.allow_empty {
            self.hint = Some("Type something, or press Escape to cancel".into());
            self.hint_is_error = true;
            cx.notify();
            return;
        }
        if let Some(check) = &self.check
            && let Err(error) = check(&text)
        {
            self.hint = Some(error.into());
            self.hint_is_error = true;
            cx.notify();
            return;
        }
        if let Some(then) = self.then.take() {
            finish(&self.workspace, text, then, window, cx);
        }
    }
}

impl Render for TextPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .key_context("WorkTextPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                v_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .gap_1()
                    // A checked prompt keeps its title above the status.
                    .when(self.check.is_some(), |this| {
                        this.child(
                            Label::new(self.title.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(self.hint.clone().unwrap_or_else(|| self.title.clone()))
                            .size(LabelSize::Small)
                            .color(if self.hint_is_error {
                                Color::Error
                            } else {
                                Color::Muted
                            }),
                    ),
            )
    }
}
