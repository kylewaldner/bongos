use std::cell::RefCell;
use std::rc::Rc;

use bongos_minibuffer::SessionView;
use editor::Editor;
use gpui::{
    Action as _, BorrowAppContext as _, Entity, TestAppContext, VisualTestContext, WindowHandle,
};
use project::{Fs as _, Project, ProjectPath};
use settings::SettingsStore;
use text::Point;
use workspace::{
    AppState, MinibufferRouting, MultiWorkspace, Pane, SaveIntent, Workspace, WorkspaceHandle as _,
};

use crate::actions::{UniversalArgument, XrefHistoryList};
use crate::prefix_arg;
use crate::xref_history::candidates;

/// A workspace on a project of three 40-line files, `one.txt`, `two.txt` and `three.txt`, none
/// open yet, with the goto stack routed to the minibuffer.
async fn setup(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, [ProjectPath; 3], VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        bongos_minibuffer::init(cx);
        cx.default_global::<MinibufferRouting>()
            .set_routes(vec!["xref_history".to_string()]);
        app_state
    });
    let fake = app_state.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    for name in ["one", "two", "three"] {
        let text = (1..=40)
            .map(|ix| format!("{name} {ix}\n"))
            .collect::<String>();
        fake.insert_file(format!("/root/{name}.txt"), text.into_bytes())
            .await;
    }
    let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let all = cx.read(|cx| workspace.file_project_paths(cx));
    let path = |name: &str| {
        all.iter()
            .find(|path| path.path.file_name() == Some(name))
            .unwrap()
            .clone()
    };
    let paths = [path("one.txt"), path("two.txt"), path("three.txt")];
    (workspace, paths, cx)
}

async fn open(
    workspace: &Entity<Workspace>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path.clone(), None, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    cx.run_until_parked();
    editor
}

/// Puts the caret at the start of `row`; a jump of ten rows or more records the old place.
fn select_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([Point::new(row, 0)..Point::new(row, 0)])
        })
    });
    cx.run_until_parked();
}

fn caret_row(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (ProjectPath, u32) {
    workspace.update(cx, |workspace, cx| {
        let item = workspace.active_item(cx).unwrap();
        let path = item.project_path(cx).unwrap();
        let editor = item.downcast::<Editor>().unwrap();
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            (
                path,
                editor.selections.newest_display(&display).head().row().0,
            )
        })
    })
}

/// Both stacks and the rows as a list shows them.
type Stacks = (Vec<usize>, Vec<usize>, Vec<(usize, Option<u32>)>);

fn stacks(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Stacks {
    cx.read(|cx| {
        let history = pane.read(cx).nav_history();
        let (back, forward) = history.stack_timestamps();
        let rows = history
            .entries_newest_first(cx)
            .into_iter()
            .map(|row| (row.timestamp, row.row))
            .collect();
        (back, forward, rows)
    })
}

fn session(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx)
    })
}

fn dispatch_named(name: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let action = cx.build_action(name, None).unwrap();
        window.dispatch_action(action, cx);
    });
    cx.run_until_parked();
}

fn list(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.dispatch_action(XrefHistoryList);
    cx.run_until_parked();
    assert!(session(workspace, cx).is_some(), "the list is open");
}

fn rows(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<(String, String)> {
    cx.read(|cx| {
        candidates(pane, cx)
            .into_iter()
            .map(|candidate| {
                (
                    candidate.primary.to_string(),
                    candidate
                        .secondary
                        .map(|text| text.to_string())
                        .unwrap_or_default(),
                )
            })
            .collect()
    })
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

fn record_item_switches(value: bool, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .editor
                    .navigation_history
                    .get_or_insert_default()
                    .record_item_switches = Some(value);
            });
        });
    });
}

/// One, then two moved to row 15, then three moved to row 30.
async fn visit_three_files(
    workspace: &Entity<Workspace>,
    paths: &[ProjectPath; 3],
    cx: &mut VisualTestContext,
) {
    open(workspace, &paths[0], cx).await;
    let two = open(workspace, &paths[1], cx).await;
    select_row(&two, 15, cx);
    let three = open(workspace, &paths[2], cx).await;
    select_row(&three, 30, cx);
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    workspace.read_with(cx, |workspace, _| workspace.active_pane().clone())
}

#[gpui::test]
async fn rows_are_newest_first_as_path_and_line_with_the_line_text(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    visit_three_files(&workspace, &paths, &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);

    let expected = [
        ("three.txt:1", "three 1"),
        ("two.txt:16", "two 16"),
        ("two.txt:1", "two 1"),
        ("one.txt:1", "one 1"),
    ];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(rows(&pane, &mut cx), expected);
}

#[gpui::test]
async fn typing_narrows_the_list_and_enter_goes_to_the_top_row(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    visit_three_files(&workspace, &paths, &mut cx).await;

    list(&workspace, &mut cx);
    cx.simulate_input("two.txt:16");
    cx.run_until_parked();
    dispatch_named("minibuffer::Confirm", &mut cx);

    assert!(session(&workspace, &mut cx).is_none());
    assert_eq!(caret_row(&workspace, &mut cx), (paths[1].clone(), 15));
}

async fn enter_on_another_item_leaves_the_stacks(record: bool, cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    record_item_switches(record, &mut cx);
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    open(&workspace, &paths[1], &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    let before = stacks(&pane, &mut cx);
    assert!(!before.2.is_empty());

    list(&workspace, &mut cx);
    // The oldest row is the place left in one when the caret jumped, row 1.
    for _ in 1..before.2.len() {
        dispatch_named("minibuffer::SelectNext", &mut cx);
    }
    dispatch_named("minibuffer::Confirm", &mut cx);

    assert_eq!(caret_row(&workspace, &mut cx), (paths[0].clone(), 0));
    assert_eq!(stacks(&pane, &mut cx), before);
}

#[gpui::test]
async fn enter_on_another_item_leaves_both_stacks_with_item_switches_recorded(
    cx: &mut TestAppContext,
) {
    enter_on_another_item_leaves_the_stacks(true, cx).await;
}

#[gpui::test]
async fn enter_on_another_item_leaves_both_stacks_with_item_switches_not_recorded(
    cx: &mut TestAppContext,
) {
    enter_on_another_item_leaves_the_stacks(false, cx).await;
}

#[gpui::test]
async fn enter_on_the_current_place_returns_at_once_and_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    select_row(&one, 0, &mut cx);
    let pane = active_pane(&workspace, &mut cx);
    let before = stacks(&pane, &mut cx);
    assert_eq!(rows(&pane, &mut cx)[1].0, "one.txt:1");

    list(&workspace, &mut cx);
    // The second row is the place the caret is at.
    dispatch_named("minibuffer::SelectNext", &mut cx);
    dispatch_named("minibuffer::Confirm", &mut cx);

    assert!(session(&workspace, &mut cx).is_none());
    assert_eq!(caret_row(&workspace, &mut cx), (paths[0].clone(), 0));
    assert_eq!(stacks(&pane, &mut cx), before);
}

#[gpui::test]
async fn a_closed_buffer_shows_no_text_and_enter_reopens_it(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    let one_id = one.entity_id();
    drop(one);
    open(&workspace, &paths[1], &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    pane.update_in(&mut cx, |pane, window, cx| {
        pane.close_item_by_id(one_id, SaveIntent::Close, window, cx)
    })
    .await
    .unwrap();
    cx.run_until_parked();
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 1);

    let shown = rows(&pane, &mut cx);
    assert!(!shown.is_empty());
    assert!(shown.iter().all(|(_, text)| text.is_empty()), "{shown:?}");
    assert_eq!(shown.last().unwrap().0, "one.txt:1");
    let before = stacks(&pane, &mut cx);

    list(&workspace, &mut cx);
    for _ in 1..shown.len() {
        dispatch_named("minibuffer::SelectNext", &mut cx);
    }
    dispatch_named("minibuffer::Confirm", &mut cx);

    assert_eq!(caret_row(&workspace, &mut cx), (paths[0].clone(), 0));
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 2);
    assert_eq!(stacks(&pane, &mut cx), before);
}

#[gpui::test]
async fn an_empty_stack_says_so_and_opens_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    open(&workspace, &paths[0], &mut cx).await;
    let messages = echo(&mut cx);

    cx.dispatch_action(XrefHistoryList);
    cx.run_until_parked();

    assert!(session(&workspace, &mut cx).is_none());
    assert_eq!(*messages.borrow(), ["Goto stack is empty"]);
}

#[gpui::test]
async fn the_command_runs_once_after_the_universal_argument(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    visit_three_files(&workspace, &paths, &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    let before = stacks(&pane, &mut cx);

    // Without this declaration the key would be repeated (the list opened once per count).
    assert!(
        cx.update(|_, cx| matches!(
            prefix_arg::meaning(cx, XrefHistoryList::name_for_type()),
            Some(prefix_arg::Meaning::Ignored)
        )),
        "the list command declares that it ignores the argument"
    );

    cx.dispatch_action(UniversalArgument);
    cx.update(|window, cx| prefix_arg::hand_off(window, cx));
    list(&workspace, &mut cx);
    cx.run_until_parked();

    assert!(
        cx.update(|window, cx| prefix_arg::pending(window, cx))
            .is_none()
    );
    assert_eq!(stacks(&pane, &mut cx), before);
}

#[gpui::test]
async fn a_closed_untitled_buffer_is_skipped_when_walking_back(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    workspace
        .update_in(&mut cx, |workspace, window, cx| {
            Editor::new_in_workspace(workspace, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let untitled = workspace.read_with(&cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).unwrap()
    });
    untitled.update_in(&mut cx, |editor, window, cx| {
        editor.set_text("line\n".repeat(40), window, cx)
    });
    select_row(&untitled, 30, &mut cx);
    let untitled_id = untitled.entity_id();
    drop(untitled);
    open(&workspace, &paths[1], &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    pane.update_in(&mut cx, |pane, window, cx| {
        pane.close_item_by_id(untitled_id, SaveIntent::Skip, window, cx)
    })
    .await
    .unwrap();
    cx.run_until_parked();

    let weak = pane.downgrade();
    workspace
        .update_in(&mut cx, |workspace, window, cx| {
            workspace.go_back(weak, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    // The untitled buffer has no path to reopen, so the walk went on to the entry in one.txt.
    assert_eq!(caret_row(&workspace, &mut cx), (paths[0].clone(), 20));
}
