//! Tests of the isearch session: the starters, typing, repeats, the failing and wrapped states,
//! Backspace, the exits and C-g, the history, the mark push, and the shipped key block. The keys
//! are the real rows of `specific-overrides.json`; every exit leaves the text byte-identical.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use bongos_minibuffer::{HistoryStore, Session, SessionView};
use editor::Editor;
use gpui::{
    Action, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
    WindowHandle,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, PathKey, ToOffset as _};
use project::Project;
use settings::KeymapFile;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::actions::{Backward, BackwardRegexp, Forward, ForwardRegexp, IN_SESSION, RepeatForward};
use crate::engine::FindOutcome;
use crate::session::{
    PENDING, REGEXP_HISTORY, TEXT_HISTORY, deliver, last_generation, test_session,
};

/// Four lines with an `x` each, at 2, 6, 10 and 14.
const XS: &str = "a x\nb x\nc x\nd x\n";

const KOTLIN: &str = "\
fun main() {
    val record = load()
    reconcile(record)
}

fun reconcile(record: Record) {
    println(record)
}
";

/// Starts the app with the minibuffer, the Emacs commands and isearch, and binds the shipped
/// overrides keymap (the minibuffer blocks and the isearch session block).
pub(crate) fn init(routed: bool, cx: &mut TestAppContext) -> std::sync::Arc<AppState> {
    let routes = if routed {
        vec![crate::session::HOST.to_string()]
    } else {
        Vec::new()
    };
    init_with_routes(Some(routes), cx)
}

/// The same with `minibuffer.enabled_for` as `routes` sets it, or left at its default for `None`.
fn init_with_routes(
    routes: Option<Vec<String>>,
    cx: &mut TestAppContext,
) -> std::sync::Arc<AppState> {
    cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_emacs::init(cx);
        crate::init(cx);
        if let Some(routes) = routes {
            cx.default_global::<MinibufferRouting>().set_routes(routes);
        }
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx)
                .expect("the overrides keymap loads");
        cx.bind_keys(bindings);
        app_state
    })
}

/// A workspace with an editor on `text` from `make`, focused, the cursor at 0.
pub(crate) async fn workspace_with(
    routed: bool,
    cx: &mut TestAppContext,
    make: impl FnOnce(&Entity<Project>, &mut gpui::Window, &mut gpui::App) -> Entity<Editor>,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let routes = routed.then(|| vec![crate::session::HOST.to_string()]);
    workspace_with_routes(Some(routes.unwrap_or_default()), cx, make).await
}

/// The same with `minibuffer.enabled_for` as `routes` sets it, or left at its default for `None`.
async fn workspace_with_routes(
    routes: Option<Vec<String>>,
    cx: &mut TestAppContext,
    make: impl FnOnce(&Entity<Project>, &mut gpui::Window, &mut gpui::App) -> Entity<Editor>,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = init_with_routes(routes, cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let editor = make(&project, window, cx);
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

/// A workspace with an editor over one buffer of `text`.
pub(crate) async fn workspace_text(
    text: &str,
    routed: bool,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let text = text.to_string();
    workspace_with(routed, cx, move |project, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&text, None, false, cx)
        });
        cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx))
    })
    .await
}

pub(crate) fn session_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

pub(crate) fn status(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<SessionView>(cx)
        })
        .expect("a session is open");
    view.read_with(cx, |view, _| view.status().to_string())
}

pub(crate) fn prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let state = test_session(workspace, cx).expect("a session is open");
    let prompt = state.borrow().prompt.clone();
    prompt
}

/// The search string, as the session read it from the input.
pub(crate) fn input(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let state = test_session(workspace, cx).expect("a session is open");
    let string = state.borrow().string.clone();
    string
}

/// Every selection as `tail..head`, so a reversed selection reads backward.
pub(crate) fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<usize>> {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor
            .selections
            .all::<MultiBufferOffset>(&display)
            .iter()
            .map(|selection| selection.tail().0..selection.head().0)
            .collect()
    })
}

/// A selection from `end` back to `start`, as [`selections`] reports it: the head at `start`.
fn reversed(start: usize, end: usize) -> Range<usize> {
    Range {
        start: end,
        end: start,
    }
}

pub(crate) fn cursor(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    let selections = selections(editor, cx);
    assert_eq!(selections.len(), 1, "one cursor: {selections:?}");
    assert!(
        selections[0].is_empty(),
        "no selection: {:?}",
        selections[0]
    );
    selections[0].start
}

pub(crate) fn set_selections(
    editor: &Entity<Editor>,
    ranges: Vec<Range<usize>>,
    cx: &mut VisualTestContext,
) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges(
                ranges
                    .into_iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end)),
            )
        })
    });
    cx.run_until_parked();
}

pub(crate) fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.text(cx))
}

fn scroll_top(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
    editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
}

pub(crate) fn ring(name: &str, cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| HistoryStore::global(cx).ring(name).entries().to_vec())
}

pub(crate) fn mark(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<usize> {
    cx.update(|_, cx| {
        let anchor = bongos_emacs::mark::mark_for(editor, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        Some(anchor.to_offset(&snapshot).0)
    })
}

/// Runs a starter in the focused editor; the session opens.
pub(crate) fn start(
    action: impl Action,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) {
    cx.dispatch_action(action);
    cx.run_until_parked();
    assert!(session_open(workspace, cx), "the session is open");
}

/// Types `string` one character at a time, each search landing before the next character.
pub(crate) fn type_slowly(string: &str, cx: &mut VisualTestContext) {
    for character in string.chars() {
        cx.simulate_input(&character.to_string());
        cx.run_until_parked();
    }
}

pub(crate) fn keys(keys: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

/// The starts of `needle` in `text`, ignoring ASCII case, as smart case does for a lowercase
/// string.
fn starts(text: &str, needle: &str) -> Vec<usize> {
    let haystack = text.to_ascii_lowercase();
    haystack
        .match_indices(needle)
        .map(|(start, _)| start)
        .collect()
}

async fn starters_open_a_session_with_their_prompt(routed: bool, cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(KOTLIN, routed, cx).await;
    let cx = &mut cx;
    let starters: [(Box<dyn Action>, &str); 4] = [
        (Box::new(Forward), "I-search: "),
        (Box::new(Backward), "I-search backward: "),
        (Box::new(ForwardRegexp), "Regexp I-search: "),
        (Box::new(BackwardRegexp), "Regexp I-search backward: "),
    ];
    for (action, expected) in starters {
        cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
        cx.run_until_parked();
        assert!(session_open(&workspace, cx), "{expected}");
        assert_eq!(in_strip(&workspace, cx), routed, "{expected}");
        assert_eq!(prompt(&workspace, cx), expected);
        assert_eq!(status(&workspace, cx), "");
        keys("ctrl-g", cx);
        assert!(!session_open(&workspace, cx), "{expected}");
        assert!(test_session(&workspace, cx).is_none());
        assert_eq!(cursor(&editor, cx), 0);
    }
    assert_eq!(text(&editor, cx), KOTLIN);
}

#[gpui::test]
async fn isearch_starters_open_a_centered_session(cx: &mut TestAppContext) {
    starters_open_a_session_with_their_prompt(false, cx).await;
}

#[gpui::test]
async fn isearch_starters_open_a_session_in_the_strip(cx: &mut TestAppContext) {
    starters_open_a_session_with_their_prompt(true, cx).await;
}

#[gpui::test]
async fn isearch_actions_are_listed_in_the_command_palette(cx: &mut TestAppContext) {
    init(true, cx);
    cx.update(|cx| {
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx)
            .expect("the palette filter is installed");
        let mut shown = 0;
        for name in cx.all_action_names() {
            if !name.starts_with("isearch::") {
                continue;
            }
            let action = cx.build_action(name, None).expect("the action builds");
            assert!(!filter.is_hidden(action.as_ref()), "{name} is hidden");
            shown += 1;
        }
        assert!(shown >= 28, "{shown} isearch actions");
    });
}

#[gpui::test]
async fn isearch_opens_in_the_strip_with_the_default_settings(cx: &mut TestAppContext) {
    let text = KOTLIN.to_string();
    let (workspace, _editor, mut cx) = workspace_with_routes(None, cx, move |project, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&text, None, false, cx)
        });
        cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx))
    })
    .await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        assert!(
            cx.global::<MinibufferRouting>()
                .is_routed(crate::session::HOST)
        );
    });
    start(Forward, &workspace, cx);
    assert!(in_strip(&workspace, cx));
    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
}

async fn typing_lands_on_the_first_match_after_point(routed: bool, cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(KOTLIN, routed, cx).await;
    let cx = &mut cx;
    let all = starts(KOTLIN, "reco");
    assert!(all.len() > 3);

    start(Forward, &workspace, cx);
    type_slowly("reco", cx);
    assert_eq!(input(&workspace, cx), "reco");
    assert_eq!(cursor(&editor, cx), all[0] + 4);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(status(&workspace, cx), format!("1/{}", all.len()));

    keys("enter", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(selections(&editor, cx), vec![all[0]..all[0] + 4]);
    assert_eq!(text(&editor, cx), KOTLIN);
}

#[gpui::test]
async fn isearch_typing_lands_on_the_first_match_centered(cx: &mut TestAppContext) {
    typing_lands_on_the_first_match_after_point(false, cx).await;
}

#[gpui::test]
async fn isearch_typing_lands_on_the_first_match_in_the_strip(cx: &mut TestAppContext) {
    typing_lands_on_the_first_match_after_point(true, cx).await;
}

#[gpui::test]
async fn isearch_searches_across_the_excerpts_of_a_multibuffer(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_with(true, cx, |project, window, cx| {
        let first = cx.new(|cx| Buffer::local("one\nskip alpha\ntwo\n", cx));
        let second = cx.new(|cx| Buffer::local("three\nfind beta\nfour\n", cx));
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_excerpts_for_path(
                PathKey::sorted(0),
                first,
                [
                    Point::new(0, 0)..Point::new(0, 3),
                    Point::new(2, 0)..Point::new(2, 3),
                ],
                0,
                cx,
            );
            multibuffer.set_excerpts_for_path(
                PathKey::sorted(1),
                second,
                [Point::new(1, 0)..Point::new(1, 9)],
                0,
                cx,
            );
            multibuffer
        });
        cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx))
    })
    .await;
    let cx = &mut cx;
    let content = text(&editor, cx);
    assert!(!content.contains("alpha"), "{content:?}");
    let beta = content.find("beta").expect("the second buffer's excerpt");

    start(Forward, &workspace, cx);
    type_slowly("beta", cx);
    assert_eq!(cursor(&editor, cx), beta + 4);
    assert_eq!(status(&workspace, cx), "1/1");
    // The text outside the excerpts is not searched.
    keys("backspace backspace backspace backspace", cx);
    type_slowly("alpha", cx);
    assert_eq!(prompt(&workspace, cx), "Failing I-search: ");
    keys("ctrl-g", cx);
    assert_eq!(input(&workspace, cx), "a", "only the failing part goes");
    keys("backspace", cx);
    type_slowly("two", cx);
    let two = content.find("two").unwrap();
    assert_eq!(cursor(&editor, cx), two + 3);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![two..two + 3]);
    assert_eq!(text(&editor, cx), content);
}

#[gpui::test]
async fn isearch_a_selection_seeds_the_search_and_is_deactivated(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(KOTLIN, false, cx).await;
    let cx = &mut cx;
    let all = starts(KOTLIN, "reconcile");
    assert_eq!(all.len(), 2);
    set_selections(&editor, vec![all[0]..all[0] + 9], cx);
    editor.update(cx, |editor, cx| editor.set_selection_mark_mode(true, cx));

    start(Forward, &workspace, cx);
    assert_eq!(input(&workspace, cx), "reconcile");
    assert!(!editor.read_with(cx, |editor, _| editor.selection_mark_mode()));
    assert_eq!(cursor(&editor, cx), all[1] + 9, "the next occurrence");
    assert_eq!(status(&workspace, cx), "2/2");

    // C-r goes back to the previous match in one press.
    keys("ctrl-r", cx);
    assert_eq!(cursor(&editor, cx), all[0]);
    assert_eq!(prompt(&workspace, cx), "I-search backward: ");
    assert_eq!(status(&workspace, cx), "1/2");

    // C-g puts the selection and mark mode back.
    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(selections(&editor, cx), vec![all[0]..all[0] + 9]);
    assert!(editor.read_with(cx, |editor, _| editor.selection_mark_mode()));

    // Backward from a selection: the previous occurrence.
    editor.update(cx, |editor, cx| editor.set_selection_mark_mode(false, cx));
    set_selections(&editor, vec![all[1]..all[1] + 9], cx);
    start(Backward, &workspace, cx);
    assert_eq!(input(&workspace, cx), "reconcile");
    assert_eq!(cursor(&editor, cx), all[0]);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![reversed(all[0], all[0] + 9)]);
    assert_eq!(text(&editor, cx), KOTLIN);
}

#[gpui::test]
async fn isearch_without_a_selection_starts_empty(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(KOTLIN, false, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![5..5], cx);
    start(Forward, &workspace, cx);
    assert_eq!(input(&workspace, cx), "");
    assert_eq!(cursor(&editor, cx), 5);
    keys("ctrl-g", cx);
}

#[gpui::test]
async fn isearch_repeats_move_to_the_next_and_previous_match(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(status(&workspace, cx), "1/4");
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(status(&workspace, cx), "2/4");
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 11);
    keys("ctrl-r", cx);
    assert_eq!(cursor(&editor, cx), 6, "backward lands on the match start");
    assert_eq!(status(&workspace, cx), "2/4");
    keys("ctrl-r", cx);
    assert_eq!(cursor(&editor, cx), 2);
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    keys("ctrl-g", cx);
    assert_eq!(cursor(&editor, cx), 0);
    assert_eq!(text(&editor, cx), XS);
}

#[gpui::test]
async fn isearch_fails_wraps_and_overwraps(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![8..8], cx);

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 11);
    assert_eq!(status(&workspace, cx), "3/4");
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 15);
    assert_eq!(status(&workspace, cx), "4/4");

    keys("ctrl-s", cx);
    assert_eq!(prompt(&workspace, cx), "Failing I-search: ");
    assert_eq!(cursor(&editor, cx), 15, "the view stays on the last match");
    assert_eq!(status(&workspace, cx), "", "no counter while failing");

    keys("ctrl-s", cx);
    assert_eq!(prompt(&workspace, cx), "Wrapped I-search: ");
    assert_eq!(cursor(&editor, cx), 3);
    assert_eq!(status(&workspace, cx), "1/4");
    keys("ctrl-s", cx);
    assert_eq!(prompt(&workspace, cx), "Wrapped I-search: ");
    assert_eq!(cursor(&editor, cx), 7);
    keys("ctrl-s", cx);
    assert_eq!(prompt(&workspace, cx), "Overwrapped I-search: ");
    assert_eq!(cursor(&editor, cx), 11);

    // C-g after a wrap returns to the exact origin.
    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(cursor(&editor, cx), 8);
    assert_eq!(text(&editor, cx), XS);
}

#[gpui::test]
async fn isearch_backspace_returns_to_the_earlier_match(cx: &mut TestAppContext) {
    let content = "rec x reco y rec z reco\n";
    let (workspace, editor, mut cx) = workspace_text(content, false, cx).await;
    let cx = &mut cx;
    let reco = starts(content, "reco");

    start(Forward, &workspace, cx);
    type_slowly("rec", cx);
    assert_eq!(cursor(&editor, cx), 3);
    type_slowly("o", cx);
    assert_eq!(cursor(&editor, cx), reco[0] + 4);
    keys("backspace", cx);
    assert_eq!(input(&workspace, cx), "rec");
    assert_eq!(cursor(&editor, cx), 3, "back on the match of rec");

    type_slowly("o", cx);
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), reco[1] + 4);
    keys("backspace", cx);
    assert_eq!(input(&workspace, cx), "rec");
    assert_eq!(cursor(&editor, cx), 3);
    keys("backspace backspace backspace", cx);
    assert_eq!(input(&workspace, cx), "");
    assert_eq!(
        cursor(&editor, cx),
        0,
        "an empty string is back at the origin"
    );
    // Backspace on an empty string does nothing.
    keys("backspace", cx);
    assert!(session_open(&workspace, cx));
    keys("enter", cx);
    assert_eq!(text(&editor, cx), content);
}

#[gpui::test]
async fn isearch_enter_selects_and_ctrl_enter_exits_at_the_other_end(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    keys("enter", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(
        selections(&editor, cx),
        vec![2..3],
        "point at the end, match selected"
    );
    assert_eq!(text(&editor, cx), XS);

    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    keys("ctrl-s ctrl-enter", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(
        selections(&editor, cx),
        vec![6..6],
        "point at the start, no selection"
    );
    assert_eq!(text(&editor, cx), XS);

    set_selections(&editor, vec![16..16], cx);
    start(Backward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 14);
    keys("enter", cx);
    assert_eq!(
        selections(&editor, cx),
        vec![reversed(14, 15)],
        "backward: point at the start"
    );

    set_selections(&editor, vec![16..16], cx);
    start(Backward, &workspace, cx);
    type_slowly("x", cx);
    keys("ctrl-enter", cx);
    assert_eq!(selections(&editor, cx), vec![15..15]);
    assert_eq!(text(&editor, cx), XS);
}

fn long_text() -> String {
    (0..120)
        .map(|row| match row {
            90 => "    val target = needle()".to_string(),
            _ => format!("// line {row}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[gpui::test]
async fn isearch_ctrl_g_trims_a_failing_suffix_then_restores_everything(cx: &mut TestAppContext) {
    let content = long_text();
    let (workspace, editor, mut cx) = workspace_text(&content, false, cx).await;
    let cx = &mut cx;
    let line = |row: usize| content.find(&format!("// line {row}\n")).unwrap();
    let cursors = vec![
        line(1) + 3..line(1) + 3,
        line(2)..line(2) + 5,
        line(3) + 1..line(3) + 1,
    ];
    set_selections(&editor, cursors.clone(), cx);
    let top = scroll_top(&editor, cx);
    let needle = content.find("needle").unwrap();

    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    assert_eq!(
        cursor(&editor, cx),
        needle + 6,
        "one cursor during the session"
    );
    assert!(
        scroll_top(&editor, cx) != top,
        "the match scrolled into view"
    );
    cx.simulate_input("zzz");
    cx.run_until_parked();
    assert_eq!(prompt(&workspace, cx), "Failing I-search: ");
    assert_eq!(cursor(&editor, cx), needle + 6);

    keys("ctrl-g", cx);
    assert!(session_open(&workspace, cx), "the first C-g only trims");
    assert_eq!(input(&workspace, cx), "needle");
    assert_eq!(prompt(&workspace, cx), "I-search: ");
    assert_eq!(cursor(&editor, cx), needle + 6);

    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(selections(&editor, cx), cursors, "every cursor is back");
    assert_eq!(scroll_top(&editor, cx), top);
    assert_eq!(text(&editor, cx), content, "C-g never edits");
    assert_eq!(mark(&editor, cx), None, "C-g pushes no mark");
    assert!(
        ring(TEXT_HISTORY, cx).is_empty(),
        "an aborted search is not recorded"
    );
}

/// RET while a find runs waits for it; a C-g that then only trims a failing suffix calls the
/// exit off, so a later click outside exits at the match without selecting it.
#[gpui::test]
async fn isearch_ctrl_g_that_trims_calls_off_a_waiting_exit(cx: &mut TestAppContext) {
    let content = long_text();
    let (workspace, editor, mut cx) = workspace_text(&content, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![4..4], cx);
    let needle = content.find("needle").unwrap();

    start(Forward, &workspace, cx);
    type_slowly("needlez", cx);
    assert_eq!(prompt(&workspace, cx), "Failing I-search: ");
    // In one go: the find for `needlezz` is still running when RET and C-g arrive.
    keys("z enter ctrl-g", cx);
    assert!(session_open(&workspace, cx), "C-g only trims");
    assert_eq!(input(&workspace, cx), "needle");
    assert_eq!(cursor(&editor, cx), needle + 6);

    let handle = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
    cx.update(|window, cx| window.focus(&handle, cx));
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
    cx.run_until_parked();
    assert!(!session_open(&workspace, cx));
    assert_eq!(
        cursor(&editor, cx),
        needle + 6,
        "no selection: not a RET exit"
    );
    assert_eq!(mark(&editor, cx), Some(4));
    assert_eq!(text(&editor, cx), content);
}

/// RET while a find runs waits for it; a key that is not the session's typed meanwhile does not
/// turn that exit into an implicit one, so the match is still selected.
#[gpui::test]
async fn isearch_a_waiting_ret_stays_a_ret_when_another_key_follows(cx: &mut TestAppContext) {
    let content = long_text();
    let (workspace, editor, mut cx) = workspace_text(&content, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![4..4], cx);
    let needle = content.find("needle").unwrap();

    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    keys("z enter ctrl-e", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(selections(&editor, cx), vec![needle..needle + 6]);
    assert_eq!(text(&editor, cx), content);
}

#[gpui::test]
async fn isearch_escape_aborts(cx: &mut TestAppContext) {
    let content = long_text();
    let (workspace, editor, mut cx) = workspace_text(&content, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![4..4], cx);
    let top = scroll_top(&editor, cx);

    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    assert!(scroll_top(&editor, cx) != top);
    keys("escape", cx);
    assert!(!session_open(&workspace, cx));
    assert_eq!(cursor(&editor, cx), 4);
    assert_eq!(scroll_top(&editor, cx), top);
    assert_eq!(text(&editor, cx), content);
    assert!(ring(TEXT_HISTORY, cx).is_empty());
}

#[gpui::test]
async fn isearch_a_click_outside_or_another_modal_exits_at_the_match(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, true, cx).await;
    let cx = &mut cx;
    set_selections(&editor, vec![1..1], cx);

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    keys("ctrl-s", cx);
    // A click on the editor: focus moves there and the strip's prompt is canceled.
    let handle = editor.read_with(cx, |editor, cx| editor.focus_handle(cx));
    cx.update(|window, cx| window.focus(&handle, cx));
    workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
    cx.run_until_parked();
    assert!(!session_open(&workspace, cx));
    assert!(test_session(&workspace, cx).is_none());
    assert_eq!(cursor(&editor, cx), 7, "at the match end, no selection");
    assert_eq!(mark(&editor, cx), Some(1), "the origin is pushed");
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["x".to_string()]);
    assert_eq!(text(&editor, cx), XS);

    // Another prompt replacing the session.
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    assert_eq!(cursor(&editor, cx), 11);
    workspace.update_in(cx, |workspace, window, cx| {
        bongos_minibuffer::open(workspace, Session::string("other", "Other: "), window, cx);
    });
    cx.run_until_parked();
    assert!(test_session(&workspace, cx).is_none());
    assert!(session_open(&workspace, cx), "the other prompt is open");
    assert_eq!(cursor(&editor, cx), 11);
    assert_eq!(mark(&editor, cx), Some(7));
    assert_eq!(text(&editor, cx), XS);
}

#[gpui::test]
async fn isearch_history_records_exits_recalls_and_walks(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;

    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    keys("enter", cx);
    assert_eq!(ring(TEXT_HISTORY, cx), vec!["x".to_string()]);

    // C-s with an empty string recalls the last search.
    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    keys("ctrl-s", cx);
    assert_eq!(input(&workspace, cx), "x");
    assert_eq!(cursor(&editor, cx), 3);
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 7);
    keys("enter", cx);

    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    type_slowly("b", cx);
    keys("enter", cx);
    assert_eq!(
        ring(TEXT_HISTORY, cx),
        vec!["b".to_string(), "x".to_string()]
    );

    // An aborted search is not recorded.
    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    type_slowly("d", cx);
    keys("ctrl-g", cx);
    assert_eq!(
        ring(TEXT_HISTORY, cx),
        vec!["b".to_string(), "x".to_string()]
    );

    // M-p and M-n walk the ring, searching each entry from the origin.
    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    type_slowly("c", cx);
    assert_eq!(cursor(&editor, cx), 9);
    keys("alt-p", cx);
    assert_eq!(input(&workspace, cx), "b");
    assert_eq!(cursor(&editor, cx), 5);
    keys("alt-p", cx);
    assert_eq!(input(&workspace, cx), "x");
    assert_eq!(cursor(&editor, cx), 3, "the first x after the origin");
    keys("alt-p", cx);
    assert_eq!(input(&workspace, cx), "x", "the ring ends there");
    keys("alt-n", cx);
    assert_eq!(input(&workspace, cx), "b");
    keys("alt-n", cx);
    assert_eq!(
        input(&workspace, cx),
        "c",
        "past the newest: what was typed"
    );
    assert_eq!(cursor(&editor, cx), 9);
    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));

    // The regexp history is separate.
    start(ForwardRegexp, &workspace, cx);
    keys("ctrl-s", cx);
    assert_eq!(input(&workspace, cx), "", "nothing to recall");
    type_slowly("[cd] x", cx);
    keys("enter", cx);
    assert_eq!(ring(REGEXP_HISTORY, cx), vec!["[cd] x".to_string()]);
    assert_eq!(
        ring(TEXT_HISTORY, cx),
        vec!["b".to_string(), "x".to_string()]
    );
    assert_eq!(text(&editor, cx), XS);
}

#[gpui::test]
async fn isearch_enter_then_pop_mark_returns_to_the_origin(cx: &mut TestAppContext) {
    let content = (0..80)
        .map(|row| match row {
            5 => "start here needle".to_string(),
            55 => "far needle".to_string(),
            _ => format!("row {row}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (workspace, editor, mut cx) = workspace_text(&content, false, cx).await;
    let cx = &mut cx;
    let origin = content.find("start here").unwrap();
    let near = content.find("needle").unwrap();
    let far = content.rfind("needle").unwrap();

    // A match on the origin's row.
    set_selections(&editor, vec![origin..origin], cx);
    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    keys("enter", cx);
    assert_eq!(selections(&editor, cx), vec![near..near + 6]);
    assert_eq!(mark(&editor, cx), Some(origin));
    cx.dispatch_action(bongos_emacs::actions::PopMark);
    cx.run_until_parked();
    assert_eq!(cursor(&editor, cx), origin);

    // A match 50 rows away.
    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    keys("ctrl-s enter", cx);
    assert_eq!(selections(&editor, cx), vec![far..far + 6]);
    cx.dispatch_action(bongos_emacs::actions::PopMark);
    cx.run_until_parked();
    assert_eq!(cursor(&editor, cx), origin);

    // C-g pushes nothing: the mark and the ring stay as they were.
    let ring_before = cx.update(|_, cx| bongos_emacs::mark::ring_for(&editor, cx).len());
    let mark_before = mark(&editor, cx);
    start(Forward, &workspace, cx);
    type_slowly("needle", cx);
    keys("ctrl-s ctrl-g", cx);
    assert_eq!(cursor(&editor, cx), origin);
    assert_eq!(mark(&editor, cx), mark_before);
    assert_eq!(
        cx.update(|_, cx| bongos_emacs::mark::ring_for(&editor, cx).len()),
        ring_before
    );
    assert_eq!(text(&editor, cx), content);
}

#[gpui::test]
async fn isearch_session_commands_pass_the_key_on_without_a_session(cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    let started = Rc::new(Cell::new(0));
    let repeated = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        let started = started.clone();
        cx.on_action(move |_: &Forward, _| started.set(started.get() + 1));
        let repeated = repeated.clone();
        cx.on_action(move |_: &RepeatForward, _| repeated.set(repeated.get() + 1));
    });

    cx.dispatch_action(RepeatForward);
    cx.run_until_parked();
    assert_eq!(repeated.get(), 1, "no session: the key goes on");
    assert!(!session_open(&workspace, cx));

    // A starter in a full editor of a workspace handles the key.
    start(Forward, &workspace, cx);
    assert_eq!(started.get(), 0);
    keys("ctrl-s", cx);
    assert_eq!(repeated.get(), 1, "the session handles its own C-s");
    keys("ctrl-g", cx);
}

#[gpui::test]
async fn isearch_a_starter_outside_a_full_editor_passes_the_key_on(cx: &mut TestAppContext) {
    init(false, cx);
    let started = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let started = started.clone();
        cx.on_action(move |_: &Forward, _| started.set(started.get() + 1));
    });

    // A single-line editor, as the query editor of another view.
    let window = cx.add_window(|window, cx| Editor::single_line(window, cx));
    let editor = window.root(cx).unwrap();
    let mut single = VisualTestContext::from_window(window.into(), cx);
    editor.update_in(&mut single, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    single.run_until_parked();
    single.dispatch_action(Forward);
    single.run_until_parked();
    assert_eq!(started.get(), 1);

    // A full editor outside a workspace has no minibuffer to search in.
    let window = cx.add_window(|window, cx| {
        let buffer = cx.new(|cx| MultiBuffer::singleton(cx.new(|cx| Buffer::local(XS, cx)), cx));
        Editor::for_multibuffer(buffer, None, window, cx)
    });
    let editor = window.root(cx).unwrap();
    let mut bare = VisualTestContext::from_window(window.into(), cx);
    editor.update_in(&mut bare, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    bare.run_until_parked();
    bare.dispatch_action(Forward);
    bare.run_until_parked();
    assert_eq!(started.get(), 2);
    assert_eq!(editor.update(&mut bare, |editor, cx| editor.text(cx)), XS);
}

#[gpui::test]
async fn isearch_typing_cancels_the_running_find(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(KOTLIN, false, cx).await;
    let cx = &mut cx;
    let all = starts(KOTLIN, "reconcile");

    start(Forward, &workspace, cx);
    // Every character is typed before any search runs: each one cancels the find before it.
    cx.simulate_input("reconcile");
    cx.run_until_parked();
    let state = test_session(&workspace, cx).unwrap();
    assert_eq!(state.borrow().stats.canceled, 8);
    assert_eq!(state.borrow().stats.inputs.len(), 9);
    assert_eq!(input(&workspace, cx), "reconcile");
    assert_eq!(cursor(&editor, cx), all[0] + 9);
    assert_eq!(status(&workspace, cx), "1/2");
    assert_ne!(status(&workspace, cx), PENDING);
    keys("ctrl-g", cx);
}

#[gpui::test]
async fn isearch_a_stale_result_never_moves_the_view(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    let state = test_session(&workspace, cx).unwrap();
    let first = last_generation(&state).unwrap();
    keys("ctrl-s", cx);
    assert_eq!(cursor(&editor, cx), 7);

    // The answer to an older request, arriving late, is dropped.
    let late = FindOutcome::Found(MultiBufferOffset(10)..MultiBufferOffset(11));
    cx.update(|window, cx| deliver(&state, first, late.clone(), window, cx));
    assert_eq!(cursor(&editor, cx), 7);
    // So is a second delivery of the newest one, which has landed already.
    let newest = last_generation(&state).unwrap();
    cx.update(|window, cx| deliver(&state, newest, late, window, cx));
    assert_eq!(cursor(&editor, cx), 7);
    assert_eq!(status(&workspace, cx), "2/4");
    keys("ctrl-g", cx);
    assert_eq!(text(&editor, cx), XS);
}

#[gpui::test]
async fn isearch_an_invalid_regexp_keeps_the_string(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    start(ForwardRegexp, &workspace, cx);
    type_slowly("b x", cx);
    assert_eq!(cursor(&editor, cx), 7);
    type_slowly("(", cx);
    assert_eq!(input(&workspace, cx), "b x(");
    assert!(
        status(&workspace, cx).starts_with("[Invalid regexp: "),
        "{}",
        status(&workspace, cx)
    );
    assert_eq!(cursor(&editor, cx), 7);
    keys("backspace", cx);
    assert_eq!(status(&workspace, cx), "1/1");
    keys("ctrl-g", cx);
    assert_eq!(text(&editor, cx), XS);
}

/// The session block of both overrides keymaps binds exactly the in-session actions, but the
/// query replace ones.
#[gpui::test]
async fn isearch_the_session_block_binds_the_in_session_actions(cx: &mut TestAppContext) {
    init(false, cx);
    for path in [
        "keymaps/specific-overrides.json",
        "keymaps/specific-overrides-macos.json",
    ] {
        let mut bound = cx.update(|cx| {
            KeymapFile::load_asset_allow_partial_failure(path, cx)
                .unwrap()
                .into_iter()
                .filter(|binding| {
                    binding
                        .predicate()
                        .is_some_and(|predicate| predicate.to_string() == "Isearch > Editor")
                })
                .map(|binding| binding.action().name())
                .collect::<Vec<_>>()
        });
        bound.sort_unstable();
        bound.dedup();
        // Query replace is not bound by isearch: its own feature binds it.
        let mut expected = IN_SESSION
            .iter()
            .copied()
            .filter(|name| !name.starts_with("isearch::QueryReplace"))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        assert_eq!(bound, expected, "{path}");
    }
}

/// A stale timer never overwrites a landed result; `PENDING` shows only while a find runs.
#[gpui::test]
async fn isearch_the_status_shows_a_running_find(cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = workspace_text(XS, false, cx).await;
    let cx = &mut cx;
    start(Forward, &workspace, cx);
    type_slowly("x", cx);
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.run_until_parked();
    assert_eq!(status(&workspace, cx), "1/4");
    keys("ctrl-g", cx);
}
