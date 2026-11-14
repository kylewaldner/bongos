use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bongos_emacs_model::kill_ring::Entry;
use bongos_minibuffer::SessionView;
use editor::{Editor, MultiBufferOffset, kill_ring};
use gpui::{
    AppContext as _, Entity, Focusable as _, KeyBinding, KeyBindingContextPredicate,
    TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use text::ToOffset as _;
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace};

use crate::actions::{BrowseKillRing, Yank, YankPop};
use crate::browse_kill_ring::{Row, candidates};
use crate::mark::mark_for;

/// A workspace with an editor on `text`, the caret at `caret`, focused, the minibuffer installed
/// and the keys the browser needs bound as the keymap files bind them.
async fn workspace_editor(
    text: &str,
    caret: usize,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        bongos_minibuffer::init(cx);
        // Routed to the strip, the presentation whose picker answers the `minibuffer` actions.
        cx.default_global::<MinibufferRouting>()
            .set_routes(vec!["kill_ring".to_string()]);
        app_state
    });
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
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(text, None, false, cx)
        });
        buffer.update(cx, |buffer, _| buffer.set_group_interval(Duration::ZERO));
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(caret)..MultiBufferOffset(caret)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-y", Yank::default(), Some("Editor")),
            KeyBinding::new("alt-y", YankPop::default(), Some("Editor")),
            KeyBinding::new("alt-shift-y", BrowseKillRing, Some("Editor")),
        ]);
        bind(cx, "enter", "minibuffer::Confirm");
        bind(cx, "ctrl-g", "minibuffer::Cancel");
        bind(cx, "alt-n", "minibuffer::SelectNext");
        bind(cx, "alt-p", "minibuffer::SelectPrev");
    });
    cx.run_until_parked();
    (workspace, editor, cx)
}

/// Binds `keys` to the action `name` inside the browser's prompt.
fn bind(cx: &mut gpui::App, keys: &str, name: &str) {
    let action = cx.build_action(name, None).unwrap();
    let predicate = KeyBindingContextPredicate::parse("KillRing > Editor").unwrap();
    let binding = KeyBinding::load(
        keys,
        action,
        Some(Rc::new(predicate)),
        false,
        None,
        cx.keyboard_mapper().as_ref(),
    )
    .unwrap();
    cx.bind_keys([binding]);
}

/// Pushes `texts`, oldest first.
fn seed(texts: &[&str], cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        for text in texts {
            kill_ring::update(cx, |ring| ring.push(Entry::plain(text.to_string())));
        }
    });
}

fn session(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx)
    })
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.text(cx))
}

fn caret(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor
            .selections
            .newest::<MultiBufferOffset>(&display)
            .head()
            .0
    })
}

fn mark(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<usize> {
    cx.update(|_, cx| {
        let anchor = mark_for(editor, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let (text_anchor, buffer) = snapshot.anchor_to_buffer_anchor(anchor)?;
        Some(text_anchor.to_offset(buffer))
    })
}

fn labels(query: &str, cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        candidates(query, cx)
            .into_iter()
            .map(|candidate| candidate.primary.to_string())
            .collect()
    })
}

fn rows(query: &str, cx: &mut VisualTestContext) -> Vec<(usize, String)> {
    cx.update(|_, cx| {
        candidates(query, cx)
            .into_iter()
            .filter_map(|candidate| {
                let row = candidate.value.downcast_ref::<Row>()?;
                Some((row.index, row.preview.clone()))
            })
            .collect()
    })
}

fn open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("alt-shift-y");
    cx.run_until_parked();
    assert!(session(workspace, cx).is_some(), "the browser is open");
}

fn echo(cx: &mut VisualTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    messages
}

#[gpui::test]
async fn sixty_rows_after_sixty_one_kills_without_the_first(cx: &mut TestAppContext) {
    let (_, _, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    let kills = (0..61).map(|ix| format!("kill {ix}")).collect::<Vec<_>>();
    seed(&kills.iter().map(String::as_str).collect::<Vec<_>>(), cx);

    let labels = labels("", cx);
    assert_eq!(labels.len(), 60);
    assert_eq!(labels[0], "kill 60");
    assert!(!labels.contains(&"kill 0".to_string()));
}

#[gpui::test]
async fn rows_are_newest_first_with_duplicates_once(cx: &mut TestAppContext) {
    let (_, _, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    seed(&["a", "b", "a", "c"], cx);

    assert_eq!(labels("", cx), ["c", "a", "b"]);
    let indexes = rows("", cx)
        .into_iter()
        .map(|row| row.0)
        .collect::<Vec<_>>();
    assert_eq!(indexes, [0, 1, 2]);
}

#[gpui::test]
async fn typing_narrows_by_substring_regardless_of_case(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    seed(&["Alpha beta", "gamma", "BETA"], cx);
    assert_eq!(labels("bEt", cx), ["BETA", "Alpha beta"]);

    open(&workspace, cx);
    cx.simulate_input("ALPH");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "Alpha beta");
}

#[gpui::test]
async fn a_twelve_line_entry_previews_ten_lines_and_the_marker(cx: &mut TestAppContext) {
    let (_, _, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    let long = (1..=12)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    seed(&[&long], cx);

    let preview = rows("", cx).remove(0).1;
    let lines = preview.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 11);
    assert_eq!(lines[9], "line 10");
    assert_eq!(lines[10], "… 2 more lines");
}

#[gpui::test]
async fn enter_inserts_at_point_and_pushes_the_mark(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("abcd", 2, cx).await;
    let cx = &mut cx;
    seed(&["x", "y"], cx);

    open(&workspace, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), "abycd");
    assert_eq!(caret(&editor, cx), 3);
    assert_eq!(mark(&editor, cx), Some(2));
    // The ring keeps its order.
    assert_eq!(labels("", cx), ["y", "x"]);
}

#[gpui::test]
async fn after_a_yank_enter_replaces_the_yanked_text(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("ab", 1, cx).await;
    let cx = &mut cx;
    seed(&["old", "new"], cx);

    cx.simulate_keystrokes("ctrl-y");
    assert_eq!(text(&editor, cx), "anewb");
    open(&workspace, cx);
    cx.simulate_keystrokes("alt-n enter");
    cx.run_until_parked();

    assert_eq!(text(&editor, cx), "aoldb");
    assert_eq!(caret(&editor, cx), 4);
}

#[gpui::test]
async fn a_yank_pop_after_the_browser_continues_from_the_chosen_entry(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    seed(&["a", "b", "c"], cx);

    cx.simulate_keystrokes("ctrl-y");
    assert_eq!(text(&editor, cx), "c");
    open(&workspace, cx);
    cx.simulate_keystrokes("alt-n enter");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "b");

    cx.simulate_keystrokes("alt-y");
    assert_eq!(text(&editor, cx), "a");
}

#[gpui::test]
async fn ctrl_g_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("abcd", 2, cx).await;
    let cx = &mut cx;
    seed(&["x", "y"], cx);

    open(&workspace, cx);
    cx.simulate_keystrokes("alt-n ctrl-g");
    cx.run_until_parked();

    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), "abcd");
    assert_eq!(caret(&editor, cx), 2);
    assert_eq!(mark(&editor, cx), None);
    assert_eq!(labels("", cx), ["y", "x"]);
}

#[gpui::test]
async fn alt_n_and_alt_p_move_the_highlight(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    seed(&["a", "b", "c"], cx);

    // c, b, a: two down is a, one up is b.
    open(&workspace, cx);
    cx.simulate_keystrokes("alt-n alt-n alt-p enter");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "b");
}

#[gpui::test]
async fn a_secret_entry_is_masked(cx: &mut TestAppContext) {
    let (_, _, mut cx) = workspace_editor("", 0, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        let mut entry = Entry::plain("hunter2".to_string());
        entry.secret = true;
        kill_ring::update(cx, |ring| ring.push(entry));
    });

    let labels = labels("", cx);
    let rows = rows("", cx);
    assert_eq!(labels.len(), 1);
    assert!(labels[0].starts_with("●●●●●●●●") && !labels[0].contains("hunter2"));
    assert!(rows[0].1.starts_with("●●●●●●●●") && !rows[0].1.contains("hunter2"));
}

#[gpui::test]
async fn an_empty_ring_echoes_and_opens_nothing(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_editor("ab", 1, cx).await;
    let cx = &mut cx;
    let messages = echo(cx);

    cx.simulate_keystrokes("alt-shift-y");
    cx.run_until_parked();

    assert_eq!(*messages.borrow(), ["Kill ring is empty"]);
    assert!(session(&workspace, cx).is_none());
    assert_eq!(text(&editor, cx), "ab");
}
