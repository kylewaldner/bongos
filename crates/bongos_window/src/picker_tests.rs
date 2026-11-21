use bongos_emacs::actions::UniversalArgument;
use editor::Editor;
use gpui::{
    AppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::Project;
use terminal_view::TerminalView;
use workspace::{AppState, MultiWorkspace, Pane, Workspace};

use crate::actions::SwitchWindow;
use crate::picker::is_picking;
use crate::test_support::{add_editor, add_terminal, split_center};

/// A workspace in a multi-workspace window, which installs the workspace's action handlers.
async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let params = cx.update(AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        bongos_emacs::init(cx);
        crate::init(cx);
    });
    let project = Project::test(params.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    (workspace, cx)
}

/// `count` center panes, each showing a new editor, with focus in the first one.
fn panes_with_editors(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> (Vec<Entity<Pane>>, Vec<Entity<Editor>>) {
    let result = cx.update(|window, cx| {
        let panes = split_center(workspace, count, window, cx);
        let editors: Vec<_> = panes
            .iter()
            .map(|pane| add_editor(workspace, pane, window, cx))
            .collect();
        window.focus(&editors[0].focus_handle(cx), cx);
        (panes, editors)
    });
    cx.run_until_parked();
    result
}

fn badges(panes: &[Entity<Pane>], cx: &mut VisualTestContext) -> Vec<Option<String>> {
    cx.update(|_, cx| {
        panes
            .iter()
            .map(|pane| pane.read(cx).badge().map(ToString::to_string))
            .collect()
    })
}

fn focused(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| pane.focus_handle(cx).contains_focused(window, cx))
}

fn picking(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| is_picking(window, cx))
}

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| editor.read(cx).text(cx))
}

fn switch(cx: &mut VisualTestContext) {
    cx.dispatch_action(SwitchWindow);
    cx.run_until_parked();
}

fn none(count: usize) -> Vec<Option<String>> {
    vec![None; count]
}

fn shown(letters: &str) -> Vec<Option<String>> {
    letters.chars().map(|c| Some(c.to_string())).collect()
}

#[gpui::test]
async fn three_panes_show_letters_and_a_letter_picks_its_pane(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    switch(cx);
    assert_eq!(badges(&panes, cx), shown("asd"));
    assert!(picking(cx));
    cx.simulate_keystrokes("d");
    cx.run_until_parked();
    assert!(focused(&panes[2], cx));
    assert_eq!(badges(&panes, cx), none(3));
    assert!(!picking(cx));
    assert_eq!(text(&editors[0], cx), "");
    assert_eq!(text(&editors[2], cx), "");
}

#[gpui::test]
async fn two_panes_switch_at_once_with_no_badge(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 2, cx);
    cx.dispatch_action(SwitchWindow);
    assert_eq!(badges(&panes, cx), none(2));
    assert!(!picking(cx));
    cx.run_until_parked();
    assert!(focused(&panes[1], cx));
    assert_eq!(badges(&panes, cx), none(2));
}

#[gpui::test]
async fn one_pane_does_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 1, cx);
    switch(cx);
    assert_eq!(badges(&panes, cx), none(1));
    assert!(!picking(cx));
    assert!(focused(&panes[0], cx));
    // The next key is not consumed.
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(text(&editors[0], cx), "x");
}

#[gpui::test]
async fn an_unmatched_key_cancels_keeps_focus_and_is_not_typed(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    switch(cx);
    assert_eq!(badges(&panes, cx), shown("asd"));
    cx.simulate_keystrokes("z");
    cx.run_until_parked();
    assert_eq!(badges(&panes, cx), none(3));
    assert!(!picking(cx));
    assert!(focused(&panes[0], cx));
    assert_eq!(text(&editors[0], cx), "");
    // Only the one key was consumed.
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    assert_eq!(text(&editors[0], cx), "x");
}

#[gpui::test]
async fn ctrl_g_cancels(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    switch(cx);
    assert_eq!(badges(&panes, cx), shown("asd"));
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert_eq!(badges(&panes, cx), none(3));
    assert!(!picking(cx));
    assert!(focused(&panes[0], cx));
    assert_eq!(text(&editors[0], cx), "");
}

#[gpui::test]
async fn a_letter_typed_in_a_terminal_picks_and_the_terminal_gets_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, terminal) = cx.update(|window, cx| {
        let panes = split_center(&workspace, 3, window, cx);
        add_editor(&workspace, &panes[0], window, cx);
        add_editor(&workspace, &panes[1], window, cx);
        let terminal: Entity<TerminalView> = add_terminal(&workspace, &panes[2], window, cx);
        window.focus(&terminal.focus_handle(cx), cx);
        (panes, terminal)
    });
    cx.run_until_parked();
    assert!(focused(&panes[2], cx));
    switch(cx);
    assert_eq!(badges(&panes, cx), shown("asd"));
    cx.simulate_keystrokes("a");
    cx.run_until_parked();
    assert!(focused(&panes[0], cx));
    assert_eq!(badges(&panes, cx), none(3));
    let sent = cx.update(|_, cx| terminal.read(cx).terminal().read(cx).keyboard_input_sent());
    assert!(!sent, "the terminal received the picking key");
    // A key typed in the terminal with no pick running does reach it.
    cx.update(|window, cx| window.focus(&terminal.focus_handle(cx), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("x");
    cx.run_until_parked();
    let sent = cx.update(|_, cx| terminal.read(cx).terminal().read(cx).keyboard_input_sent());
    assert!(sent, "the terminal got no input with no pick running");
}

#[gpui::test]
async fn a_zoomed_frame_is_unzoomed_and_a_cancel_zooms_it_again(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    cx.update(|window, cx| window.focus(&editors[2].focus_handle(cx), cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        panes[2].update(cx, |pane, cx| pane.zoom_in(&workspace::ZoomIn, window, cx));
    });
    cx.run_until_parked();
    assert!(cx.update(|_, cx| panes[2].read(cx).is_zoomed()));
    switch(cx);
    assert!(cx.update(|_, cx| panes.iter().all(|pane| !pane.read(cx).is_zoomed())));
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_none()));
    assert_eq!(badges(&panes, cx), shown("asd"));
    cx.simulate_keystrokes("z");
    cx.run_until_parked();
    assert_eq!(badges(&panes, cx), none(3));
    assert!(cx.update(|_, cx| panes[2].read(cx).is_zoomed()));
    assert!(!cx.update(|_, cx| panes[0].read(cx).is_zoomed()));
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_some()));
    assert!(focused(&panes[2], cx));
}

#[gpui::test]
async fn a_prefix_argument_shows_the_letters_once(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        cx.bind_keys([
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
            KeyBinding::new("ctrl-x o", SwitchWindow, Some("Editor")),
        ]);
    });
    let (panes, origin) = cx.update(|window, cx| {
        let panes = split_center(&workspace, 3, window, cx);
        add_editor(&workspace, &panes[1], window, cx);
        add_editor(&workspace, &panes[2], window, cx);
        let origin = cx.new(|cx| Editor::multi_line(window, cx));
        workspace.update(cx, |workspace, cx| {
            workspace.add_item(
                panes[0].clone(),
                Box::new(origin.clone()),
                None,
                true,
                true,
                window,
                cx,
            );
        });
        window.focus(&origin.focus_handle(cx), cx);
        (panes, origin)
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-u ctrl-x o");
    cx.run_until_parked();
    assert_eq!(badges(&panes, cx), shown("asd"));
    cx.simulate_keystrokes("d");
    cx.run_until_parked();
    assert!(focused(&panes[2], cx));
    assert_eq!(badges(&panes, cx), none(3));
    assert!(!picking(cx), "a second pick started");
    assert_eq!(text(&origin, cx), "");
    let target = cx.update(|_, cx| {
        panes[2]
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("an editor")
    });
    // No second overlay waits for a key: the next one is typed.
    cx.simulate_keystrokes("q");
    cx.run_until_parked();
    assert_eq!(text(&target, cx), "q");
}
