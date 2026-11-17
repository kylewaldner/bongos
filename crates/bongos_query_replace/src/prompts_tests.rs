//! Tests of the two prompts of a query replace: what the FROM starts as, the last pair, the four
//! history rings, the refusals that keep the prompt open, M-c and C-q. The keys are the real rows
//! of `specific-overrides.json`.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use bongos_isearch_model::CaseMode;
use bongos_minibuffer::{HistoryStore, SessionView};
use editor::Editor;
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use multi_buffer::MultiBufferOffset;
use project::Project;
use settings::KeymapFile;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::prompts::{
    Answer, FROM_HISTORY, Mode, REGEXP_FROM_HISTORY, REGEXP_TO_HISTORY, TO_HISTORY,
    edit_replacement, prompt, test_prompt,
};

const TEXT: Mode = Mode {
    regexp: false,
    interactive: true,
};
const REGEXP: Mode = Mode {
    regexp: true,
    interactive: true,
};

type Log = Rc<RefCell<Vec<Answer>>>;

async fn workspace_text(
    text: &str,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        cx.default_global::<MinibufferRouting>()
            .set_routes(vec![crate::prompts::HOST.to_string()]);
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx)
                .expect("the overrides keymap loads");
        cx.bind_keys(bindings);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let text = text.to_string();
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&text, None, false, cx)
        });
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

fn select(editor: &Entity<Editor>, range: Range<usize>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([MultiBufferOffset(range.start)..MultiBufferOffset(range.end)])
        })
    });
    cx.run_until_parked();
}

fn push(name: &str, entry: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| HistoryStore::global(cx).push(name, entry, cx));
}

fn ring(name: &str, cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| HistoryStore::global(cx).ring(name).entries().to_vec())
}

fn ask(
    mode: Mode,
    workspace: &Entity<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut VisualTestContext,
) -> Log {
    let log = Log::default();
    let answers = log.clone();
    workspace.update_in(cx, |workspace, window, cx| {
        prompt(
            workspace,
            editor,
            mode,
            CaseMode::Smart,
            move |answer, _, _| answers.borrow_mut().push(answer),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    log
}

fn is_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn status(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx)
        })
        .expect("a prompt is open");
    view.read_with(cx, |view, _| view.status().to_string())
}

/// The text of the open prompt's input and its prompt line.
fn shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (String, String) {
    let id = workspace.entity_id();
    cx.update(|_, cx| test_prompt(id, cx))
        .expect("a prompt is open")
}

fn input(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    shown(workspace, cx).0
}

fn confirm(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

#[gpui::test]
async fn the_from_starts_as_a_one_line_region(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\ngamma delta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 6..10, cx);
    ask(TEXT, &workspace, &editor, cx);
    assert_eq!(
        shown(&workspace, cx),
        ("beta".into(), "Query replace: ".into())
    );
    // It is selected: typing replaces it.
    cx.simulate_input("z");
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "z");
}

#[gpui::test]
async fn a_region_over_lines_is_not_the_from(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\ngamma delta\n", cx).await;
    let cx = &mut cx;
    // The cursor, at the end of the region, is in `gamma`.
    select(&editor, 3..14, cx);
    ask(TEXT, &workspace, &editor, cx);
    assert_eq!(input(&workspace, cx), "gamma");
}

#[gpui::test]
async fn the_from_starts_as_the_symbol_at_point(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    ask(REGEXP, &workspace, &editor, cx);
    assert_eq!(
        shown(&workspace, cx),
        ("alpha".into(), "Query replace regexp: ".into())
    );
}

#[gpui::test]
async fn the_from_starts_as_the_last_from_without_a_symbol(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("a  b\n", cx).await;
    let cx = &mut cx;
    push(FROM_HISTORY, "older", cx);
    push(FROM_HISTORY, "newer", cx);
    select(&editor, 2..2, cx);
    ask(TEXT, &workspace, &editor, cx);
    assert_eq!(input(&workspace, cx), "newer");
}

#[gpui::test]
async fn an_empty_from_is_refused(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("a  b\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    assert_eq!(input(&workspace, cx), "");
    confirm(cx);
    assert!(is_open(&workspace, cx));
    assert_eq!(status(&workspace, cx), "Nothing to replace");
    assert!(log.borrow().is_empty());
}

#[gpui::test]
async fn return_accepts_the_defaults_and_the_pair_is_remembered(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    confirm(cx);
    assert_eq!(
        shown(&workspace, cx),
        (String::new(), "Query replace alpha with: ".into())
    );
    cx.simulate_input("omega");
    confirm(cx);
    assert!(!is_open(&workspace, cx));
    assert_eq!(
        *log.borrow(),
        [Answer {
            from: "alpha".into(),
            to: "omega".into(),
            regexp: false,
            case: CaseMode::Smart,
        }]
    );

    // The same FROM again: the last TO is the default, accepted with RET.
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    confirm(cx);
    assert_eq!(input(&workspace, cx), "omega");
    confirm(cx);
    assert_eq!(log.borrow()[0].to, "omega");

    // Another FROM: the TO starts empty, and an empty TO is valid.
    select(&editor, 6..10, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    confirm(cx);
    assert_eq!(input(&workspace, cx), "");
    confirm(cx);
    assert_eq!(log.borrow()[0].to, "");
}

#[gpui::test]
async fn the_pairs_keep_four_rings(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    ask(TEXT, &workspace, &editor, cx);
    confirm(cx);
    cx.simulate_input("one");
    confirm(cx);
    assert_eq!(ring(FROM_HISTORY, cx), ["alpha"]);
    assert_eq!(ring(TO_HISTORY, cx), ["one"]);
    assert!(ring(REGEXP_FROM_HISTORY, cx).is_empty());
    assert!(ring(REGEXP_TO_HISTORY, cx).is_empty());

    select(&editor, 6..10, cx);
    ask(REGEXP, &workspace, &editor, cx);
    confirm(cx);
    cx.simulate_input("two");
    confirm(cx);
    assert_eq!(ring(REGEXP_FROM_HISTORY, cx), ["beta"]);
    assert_eq!(ring(REGEXP_TO_HISTORY, cx), ["two"]);
    assert_eq!(ring(FROM_HISTORY, cx), ["alpha"]);
    assert_eq!(ring(TO_HISTORY, cx), ["one"]);

    // The non-interactive pair shares the rings of its mode.
    select(&editor, 0..5, cx);
    ask(
        Mode {
            regexp: false,
            interactive: false,
        },
        &workspace,
        &editor,
        cx,
    );
    assert_eq!(shown(&workspace, cx).1, "Replace string: ");
    cx.simulate_input("beta");
    confirm(cx);
    assert_eq!(shown(&workspace, cx).1, "Replace string beta with: ");
    confirm(cx);
    assert_eq!(ring(FROM_HISTORY, cx), ["beta", "alpha"]);
}

#[gpui::test]
async fn an_invalid_regexp_stays_at_the_prompt(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(REGEXP, &workspace, &editor, cx);
    cx.simulate_input("a(");
    confirm(cx);
    assert!(is_open(&workspace, cx));
    assert!(
        status(&workspace, cx).starts_with("Invalid regexp: "),
        "{}",
        status(&workspace, cx)
    );
    assert!(!status(&workspace, cx).contains('\n'));
    assert_eq!(shown(&workspace, cx).1, "Query replace regexp: ");
    assert!(log.borrow().is_empty());
}

#[gpui::test]
async fn a_group_beyond_the_captures_stays_at_the_prompt(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(REGEXP, &workspace, &editor, cx);
    cx.simulate_input("(a)lpha");
    confirm(cx);
    cx.simulate_input("\\9");
    confirm(cx);
    assert!(is_open(&workspace, cx));
    assert!(
        status(&workspace, cx).contains("\\9"),
        "{}",
        status(&workspace, cx)
    );
    assert!(log.borrow().is_empty());

    // A malformed escape too; a group that exists is fine.
    cx.dispatch_action(editor::actions::SelectAll);
    cx.simulate_input("\\q");
    confirm(cx);
    assert!(is_open(&workspace, cx));
    cx.dispatch_action(editor::actions::SelectAll);
    cx.simulate_input("[\\1]");
    confirm(cx);
    assert!(!is_open(&workspace, cx));
    assert_eq!(log.borrow()[0].to, "[\\1]");
}

#[gpui::test]
async fn text_mode_takes_backslashes_literally(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    confirm(cx);
    cx.simulate_input("\\9");
    confirm(cx);
    assert_eq!(log.borrow()[0].to, "\\9");
}

#[gpui::test]
async fn quoted_insert_puts_a_newline_in_the_from(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("a  b\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    cx.simulate_input("a");
    cx.simulate_keystrokes("ctrl-q ctrl-j");
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "a\u{23ce}");
    assert!(is_open(&workspace, cx), "C-j did not confirm");
    // It disarmed: Enter confirms again.
    cx.simulate_keystrokes("ctrl-q enter");
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "a\u{23ce}\u{23ce}");
    cx.simulate_keystrokes("ctrl-q x");
    cx.run_until_parked();
    assert_eq!(input(&workspace, cx), "a\u{23ce}\u{23ce}x");
    cx.dispatch_action(editor::actions::Backspace);
    cx.dispatch_action(editor::actions::Backspace);
    confirm(cx);
    assert_eq!(shown(&workspace, cx).1, "Query replace a\u{23ce} with: ");
    cx.simulate_input("b");
    confirm(cx);
    assert_eq!(log.borrow()[0].from, "a\n");
}

#[gpui::test]
async fn m_c_toggles_the_case_in_the_prompt(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    select(&editor, 2..2, cx);
    let log = ask(TEXT, &workspace, &editor, cx);
    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    assert_eq!(shown(&workspace, cx).1, "Query replace (case-sensitive): ");
    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    assert_eq!(
        shown(&workspace, cx).1,
        "Query replace (case-insensitive): "
    );
    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    confirm(cx);
    assert_eq!(
        shown(&workspace, cx).1,
        "Query replace (case-sensitive) alpha with: "
    );
    cx.simulate_keystrokes("alt-c");
    cx.run_until_parked();
    assert_eq!(
        shown(&workspace, cx).1,
        "Query replace (case-insensitive) alpha with: "
    );
    confirm(cx);
    assert_eq!(log.borrow()[0].case, CaseMode::Insensitive);
}

#[gpui::test]
async fn the_replacement_can_be_edited(cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = workspace_text("alpha beta\n", cx).await;
    let cx = &mut cx;
    let edited = Rc::new(RefCell::new(Vec::new()));
    let log = edited.clone();
    workspace.update_in(cx, |workspace, window, cx| {
        edit_replacement(
            workspace,
            REGEXP,
            CaseMode::Smart,
            "(a)",
            "x\\1",
            move |to, _, _| log.borrow_mut().push(to),
            |_, _| {},
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        shown(&workspace, cx),
        ("x\\1".into(), "Edit replacement string: ".into())
    );
    cx.simulate_input("\\2");
    confirm(cx);
    assert!(is_open(&workspace, cx), "\\2 names no group");
    cx.dispatch_action(editor::actions::SelectAll);
    cx.simulate_input("y\\1");
    confirm(cx);
    assert_eq!(*edited.borrow(), ["y\\1"]);
}
