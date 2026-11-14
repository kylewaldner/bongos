//! The two prompts of a query replace, in the minibuffer: the string to replace (FROM) and what
//! to replace it with (TO). FROM starts from the region, the symbol at point or the last FROM; TO
//! starts from the last TO when the FROM is the last FROM. M-c switches the case of the search and
//! C-q arms the prompt so that Enter or C-j puts a newline in the string.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use bongos_isearch::engine::build_query;
use bongos_isearch_model::{CaseMode, SearchSpec, compile, from_input, to_input};
use bongos_minibuffer::{HistoryStore, Outcome, Session, SessionHandle};
use bongos_query_replace_model::template;
use editor::Editor;
use gpui::{App, Context, Entity, EntityId, Keystroke, SharedString, Subscription, Window};
use language::CharKind;
use multi_buffer::MultiBufferOffset;
use project::search::SearchQuery;
use workspace::Workspace;

use crate::actions::{QuotedInsert, ToggleCase};

/// The minibuffer host of the prompts.
pub const HOST: &str = "query_replace";
/// The key context the prompts declare.
pub const KEY_CONTEXT: &str = "QueryReplacePrompt";
/// The history of text FROM strings, shared by `query-replace` and `replace-string`.
pub const FROM_HISTORY: &str = "query-replace-from";
/// The history of text replacements.
pub const TO_HISTORY: &str = "query-replace-to";
/// The history of regexp FROM strings, shared by `query-replace-regexp` and `replace-regexp`.
pub const REGEXP_FROM_HISTORY: &str = "query-replace-regexp-from";
/// The history of regexp replacements.
pub const REGEXP_TO_HISTORY: &str = "query-replace-regexp-to";

/// Which command the prompts are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// The FROM is a regexp and the TO may name its groups.
    pub regexp: bool,
    /// The command asks at every match (`query-replace`), not `replace-string`.
    pub interactive: bool,
}

/// What the prompts ask for, once both are answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub from: String,
    pub to: String,
    pub regexp: bool,
    pub case: CaseMode,
}

type Done = Box<dyn FnOnce(Answer, &mut Window, &mut App)>;
type Cancelled = Box<dyn FnOnce(&mut Window, &mut App)>;

/// The history ring that holds the FROM strings of `regexp` mode.
pub fn from_history(regexp: bool) -> &'static str {
    if regexp {
        REGEXP_FROM_HISTORY
    } else {
        FROM_HISTORY
    }
}

/// The history ring that holds the replacements of `regexp` mode.
pub fn to_history(regexp: bool) -> &'static str {
    if regexp {
        REGEXP_TO_HISTORY
    } else {
        TO_HISTORY
    }
}

#[derive(Clone)]
enum Stage {
    From,
    /// The replacement for this FROM (as the input shows it); `edit` when it replaces one
    /// already chosen.
    To {
        from: String,
        edit: bool,
    },
}

/// One open prompt.
struct Prompt {
    stage: Stage,
    mode: Mode,
    case: CaseMode,
    /// What the input holds now, as the input shows it (a newline is `⏎`).
    input: String,
    /// The newest FROM of the history when the FROM prompt opened.
    last_from: Option<String>,
    /// Whether the next key is inserted as typed (C-q).
    armed: bool,
    handle: Option<SessionHandle>,
    _intercept: Option<Subscription>,
}

type Shared = Rc<RefCell<Prompt>>;

/// The open prompt of each workspace.
#[derive(Default)]
struct Prompts(HashMap<EntityId, Shared>);

impl gpui::Global for Prompts {}

/// Registers the prompt's actions on every workspace.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleCase, window, cx| {
            match open_prompt(workspace, cx) {
                Some(prompt) => toggle_case(&prompt, window, cx),
                None => cx.propagate(),
            }
        });
        workspace.register_action(|workspace, _: &QuotedInsert, _, cx| {
            match open_prompt(workspace, cx) {
                Some(prompt) => prompt.borrow_mut().armed = true,
                None => cx.propagate(),
            }
        });
    })
    .detach();
}

/// The input and the prompt line of the open prompt of the workspace `id`.
#[cfg(test)]
pub(crate) fn test_prompt(id: EntityId, cx: &App) -> Option<(String, String)> {
    let prompt = cx.try_global::<Prompts>()?.0.get(&id)?.borrow();
    Some((prompt.input.clone(), prompt_text(&prompt)))
}

fn open_prompt(_: &mut Workspace, cx: &mut Context<Workspace>) -> Option<Shared> {
    let id = cx.entity_id();
    cx.try_global::<Prompts>()
        .and_then(|prompts| prompts.0.get(&id).cloned())
}

/// Asks for the FROM and then the TO of a replace in `editor`, and calls `on_done` with both.
/// The FROM starts as the one-line region, else the word at the cursor, else the last FROM. A
/// prompt that is cancelled calls nothing.
pub fn prompt(
    workspace: &mut Workspace,
    editor: &Entity<Editor>,
    mode: Mode,
    case: CaseMode,
    on_done: impl FnOnce(Answer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ring = HistoryStore::global(cx).ring(from_history(mode.regexp));
    let last_from = ring.get(0).map(str::to_string);
    let initial = editor
        .update(cx, |editor, cx| seed(editor, cx))
        .map(|seed| to_input(&seed))
        .or_else(|| last_from.clone())
        .unwrap_or_default();
    let state = Prompt {
        stage: Stage::From,
        mode,
        case,
        input: initial.clone(),
        last_from,
        armed: false,
        handle: None,
        _intercept: None,
    };
    open_stage(
        workspace,
        state,
        initial,
        Box::new(on_done),
        None,
        window,
        cx,
    );
}

/// Asks for a new replacement, starting from `current`, for the `from` string (as typed in the
/// prompt) of a session that is running, and calls `on_done` with it, or `on_cancel` when the
/// prompt is cancelled. The `e` key of the decision view opens this.
#[allow(clippy::too_many_arguments)]
pub fn edit_replacement(
    workspace: &mut Workspace,
    mode: Mode,
    case: CaseMode,
    from: &str,
    current: &str,
    on_done: impl FnOnce(String, &mut Window, &mut App) + 'static,
    on_cancel: impl FnOnce(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let state = Prompt {
        stage: Stage::To {
            from: to_input(from),
            edit: true,
        },
        mode,
        case,
        input: to_input(current),
        last_from: None,
        armed: false,
        handle: None,
        _intercept: None,
    };
    let done: Done = Box::new(move |answer, window, cx| on_done(answer.to, window, cx));
    open_stage(
        workspace,
        state,
        to_input(current),
        done,
        Some(Box::new(on_cancel)),
        window,
        cx,
    );
}

/// The text that starts the FROM: the region when it is within one line, else the word at the
/// cursor.
fn seed(editor: &Editor, cx: &mut App) -> Option<String> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let display = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<MultiBufferOffset>(&display);
    if selection.start != selection.end {
        let text: String = snapshot
            .text_for_range(selection.start..selection.end)
            .collect();
        if !text.contains('\n') {
            return Some(text);
        }
    }
    let (range, kind) = snapshot.surrounding_word(selection.head(), None);
    if kind != Some(CharKind::Word) || range.is_empty() {
        return None;
    }
    Some(snapshot.text_for_range(range).collect())
}

fn head(mode: Mode) -> &'static str {
    match (mode.interactive, mode.regexp) {
        (true, false) => "Query replace",
        (true, true) => "Query replace regexp",
        (false, false) => "Replace string",
        (false, true) => "Replace regexp",
    }
}

fn prompt_text(prompt: &Prompt) -> String {
    let case = match prompt.case {
        CaseMode::Smart => "",
        CaseMode::Sensitive => " (case-sensitive)",
        CaseMode::Insensitive => " (case-insensitive)",
    };
    match &prompt.stage {
        Stage::From => format!("{}{case}: ", head(prompt.mode)),
        Stage::To { edit: true, .. } => format!("Edit replacement string{case}: "),
        Stage::To { from, .. } => format!("{}{case} {from} with: ", head(prompt.mode)),
    }
}

/// The number of capture groups of the regexp of `from`, or why it does not compile.
fn groups(from: &str, regexp: bool) -> Result<usize, String> {
    let mut spec = SearchSpec::new(regexp);
    spec.string = from.to_string();
    spec.lax = false;
    let query = build_query(&compile(&spec)).map_err(|error| error.0)?;
    Ok(match &*query {
        SearchQuery::Regex { regex, .. } => regex.captures_len() - 1,
        _ => 0,
    })
}

fn check(prompt: &Prompt, input: &str) -> Result<(), SharedString> {
    let regexp = prompt.mode.regexp;
    match &prompt.stage {
        Stage::From => {
            let from = from_input(input);
            if from.is_empty() {
                return Err("Nothing to replace".into());
            }
            groups(&from, regexp)
                .map(drop)
                .map_err(|message| invalid(&message))
        }
        Stage::To { from, .. } => {
            let template = template::parse(&from_input(input), regexp)
                .map_err(|error| SharedString::from(error.to_string()))?;
            let groups = groups(&from_input(from), regexp).map_err(|message| invalid(&message))?;
            let used = template.max_group();
            if used > groups {
                return Err(format!(
                    "Invalid reference \\{used} in replacement: the regexp has {groups} group{}",
                    if groups == 1 { "" } else { "s" }
                )
                .into());
            }
            Ok(())
        }
    }
}

fn invalid(message: &str) -> SharedString {
    format!("Invalid regexp: {}", message.lines().next().unwrap_or("")).into()
}

fn toggle_case(prompt: &Shared, window: &mut Window, cx: &mut App) {
    let (text, handle) = {
        let mut prompt = prompt.borrow_mut();
        let string = match &prompt.stage {
            Stage::From => from_input(&prompt.input),
            Stage::To { from, .. } => from_input(from),
        };
        let mut spec = SearchSpec::new(prompt.mode.regexp);
        spec.string = string;
        spec.lax = false;
        spec.case = prompt.case;
        spec.toggle_case();
        prompt.case = spec.case;
        (prompt_text(&prompt), prompt.handle.clone())
    };
    if let Some(handle) = handle {
        handle.set_prompt(text, window, cx);
    }
}

fn forget(id: EntityId, prompt: &Shared, cx: &mut App) {
    let prompts = cx.default_global::<Prompts>();
    if prompts
        .0
        .get(&id)
        .is_some_and(|open| Rc::ptr_eq(open, prompt))
    {
        prompts.0.remove(&id);
    }
}

fn open_stage(
    workspace: &mut Workspace,
    state: Prompt,
    initial: String,
    done: Done,
    cancelled: Option<Cancelled>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let id = cx.entity_id();
    let weak_workspace = cx.entity().downgrade();
    let text = prompt_text(&state);
    let mode = state.mode;
    let is_from = matches!(state.stage, Stage::From);
    let ring = if is_from {
        from_history(mode.regexp)
    } else {
        to_history(mode.regexp)
    };
    let shared: Shared = Rc::new(RefCell::new(state));
    let mut session = Session::string(HOST, text)
        .history(ring)
        .initial_input(initial.clone())
        .key_context(KEY_CONTEXT)
        .check({
            let prompt = Rc::downgrade(&shared);
            move |input| match prompt.upgrade() {
                Some(prompt) => check(&prompt.borrow(), input),
                None => Ok(()),
            }
        })
        .on_input({
            let prompt = Rc::downgrade(&shared);
            move |text, _, _, _| {
                if let Some(prompt) = prompt.upgrade() {
                    prompt.borrow_mut().input = text.to_string();
                }
            }
        })
        .on_confirm({
            let prompt = Rc::downgrade(&shared);
            move |outcome, window, cx| {
                let (Some(prompt), Outcome::Input(input)) = (prompt.upgrade(), outcome) else {
                    return;
                };
                forget(id, &prompt, cx);
                confirmed(&prompt, input, &weak_workspace, done, window, cx);
            }
        })
        .on_cancel({
            let prompt = Rc::downgrade(&shared);
            move |window, cx| {
                if let Some(prompt) = prompt.upgrade() {
                    forget(id, &prompt, cx);
                }
                if let Some(cancelled) = cancelled {
                    cancelled(window, cx);
                }
            }
        });
    if !initial.is_empty() {
        session = session.select_initial_input();
    }

    // Registered before it opens: a prompt this one replaces is cancelled while it opens.
    cx.default_global::<Prompts>().0.insert(id, shared.clone());
    let handle = bongos_minibuffer::open(workspace, session, window, cx);
    let mut state = shared.borrow_mut();
    state.handle = Some(handle);
    state._intercept = Some(install_interceptor(
        Rc::downgrade(&shared),
        window.window_handle(),
        cx,
    ));
}

/// The prompt was answered with `input`: the FROM opens the TO, the TO ends the asking.
fn confirmed(
    prompt: &Shared,
    input: String,
    workspace: &gpui::WeakEntity<Workspace>,
    done: Done,
    window: &mut Window,
    cx: &mut App,
) {
    let (stage, mode, case, last_from) = {
        let prompt = prompt.borrow();
        (
            prompt.stage.clone(),
            prompt.mode,
            prompt.case,
            prompt.last_from.clone(),
        )
    };
    match stage {
        Stage::From => {
            let initial = if last_from.as_deref() == Some(input.as_str()) {
                HistoryStore::global(cx)
                    .ring(to_history(mode.regexp))
                    .get(0)
                    .map(str::to_string)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let state = Prompt {
                stage: Stage::To {
                    from: input,
                    edit: false,
                },
                mode,
                case,
                input: initial.clone(),
                last_from,
                armed: false,
                handle: None,
                _intercept: None,
            };
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    open_stage(workspace, state, initial, done, None, window, cx)
                });
            }
        }
        Stage::To { from, .. } => done(
            Answer {
                from: from_input(&from),
                to: from_input(&input),
                regexp: mode.regexp,
                case,
            },
            window,
            cx,
        ),
    }
}

/// Watches the keys of the window for the one that follows C-q: Enter and C-j put a newline in
/// the input, and any other key goes on as usual (a printable one types itself).
fn install_interceptor(
    prompt: Weak<RefCell<Prompt>>,
    window: gpui::AnyWindowHandle,
    cx: &mut App,
) -> Subscription {
    cx.intercept_keystrokes(move |event, current, cx| {
        let Some(prompt) = prompt.upgrade() else {
            return;
        };
        if !prompt.borrow().armed
            || event.keystroke.key.is_empty()
            || current.window_handle() != window
            || !event
                .context_stack
                .iter()
                .any(|context| context.contains(KEY_CONTEXT))
        {
            return;
        }
        let (handle, input) = {
            let mut prompt = prompt.borrow_mut();
            prompt.armed = false;
            (prompt.handle.clone(), prompt.input.clone())
        };
        if is_newline(&event.keystroke)
            && let Some(handle) = handle
        {
            cx.stop_propagation();
            handle.set_input(&format!("{input}{}", to_input("\n")), current, cx);
        }
    })
}

fn is_newline(key: &Keystroke) -> bool {
    let modifiers = &key.modifiers;
    if modifiers.alt || modifiers.platform || modifiers.function {
        return false;
    }
    (key.key == "enter" && !modifiers.control) || (key.key == "j" && modifiers.control)
}
