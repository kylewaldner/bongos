//! Tests of killing a buffer from the switcher, of the recently closed rows and of the M-o pane
//! entries.

use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc};

use bongos_minibuffer::{Candidate, ReadKeyView, SessionHandle};
use bongos_window::test_support::split_center;
use editor::Editor;
use fs::FakeFs;
use gpui::{
    App, AppContext as _, BorrowAppContext as _, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, IntoElement, Render, SharedString, Task, TestAppContext,
    VisualTestContext, Window, WindowHandle, div,
};
use project::{Fs as _, Project, ProjectPath};
use serde_json::json;
use settings::{KeymapFile, SettingsStore};
use workspace::{
    AppState, MinibufferRouting, MultiWorkspace, Pane, SaveIntent, Workspace, WorkspaceHandle as _,
    item::{Item, ItemEvent, SaveOptions},
};
use zed_actions::minibuffer;

use crate::actions::KillBuffer;
use crate::goto_file_open::FileTarget;
use crate::switcher::{HOST, SwitchTarget, candidates, start};
use crate::switcher_kill::current;

async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext, Arc<FakeFs>) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx)
                .expect("the overrides keymap loads");
        cx.bind_keys(bindings);
        params
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec![HOST.to_string()]);
            });
        });
    });
    let fs = params.fs.as_fake();
    fs.insert_tree(
        "/root",
        json!({
            "a.txt": "a\n",
            "b.txt": "b\n",
            "c.txt": "c\n",
            "keep1.txt": "keep one\n",
            "keep2.txt": "keep two\n",
            "moved.txt": "moved\n",
        }),
    )
    .await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window holds a workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx, fs)
}

/// An item that is dirty and cannot be saved.
struct FailingItem {
    focus_handle: FocusHandle,
}

impl EventEmitter<ItemEvent> for FailingItem {}

impl Focusable for FailingItem {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FailingItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Item for FailingItem {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Failing.kt".into()
    }

    fn is_dirty(&self, _: &App) -> bool {
        true
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _: SaveOptions,
        _: Entity<Project>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        Task::ready(Err(anyhow::anyhow!("disk full")))
    }
}

fn project_path(
    workspace: &Entity<Workspace>,
    rel: &str,
    cx: &mut VisualTestContext,
) -> ProjectPath {
    cx.read(|cx| workspace.file_project_paths(cx))
        .into_iter()
        .find(|path| path.path.as_unix_str() == rel)
        .unwrap_or_else(|| panic!("{rel} is in the worktree"))
}

/// Opens `rel` in `pane` as its active item, and returns the editor.
async fn open(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    rel: &str,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let path = project_path(workspace, rel, cx);
    let weak = pane.downgrade();
    let task = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path(path, Some(weak), true, window, cx)
    });
    let item = task.await.expect("the file opens");
    cx.run_until_parked();
    cx.read(|cx| item.act_as::<Editor>(cx))
        .expect("a file opens as an editor")
}

/// Types into `editor`, which makes it dirty.
fn edit(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| editor.update(cx, |editor, cx| editor.insert("X", window, cx)));
    cx.run_until_parked();
}

/// Puts the active item of `pane` on its navigation history, as leaving it with a jump would.
fn record_in_history(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        let editor = pane
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("the active item is an editor");
        let item: Arc<dyn workspace::item::WeakItemHandle + Send + Sync> =
            Arc::new(editor.downgrade());
        pane.update(cx, |pane, cx| {
            pane.nav_history_mut()
                .push::<()>(None, item, false, None, cx)
        });
    });
}

fn close(pane: &Entity<Pane>, item: EntityId, cx: &mut VisualTestContext) {
    let task = pane.update_in(cx, |pane, window, cx| {
        pane.close_item_by_id(item, SaveIntent::Skip, window, cx)
    });
    cx.run_until_parked();
    drop(task);
}

fn items(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<EntityId> {
    cx.read(|cx| pane.read(cx).items().map(|item| item.item_id()).collect())
}

fn is_dirty(pane: &Entity<Pane>, item: EntityId, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| {
        pane.read(cx)
            .items()
            .find(|candidate| candidate.item_id() == item)
            .is_some_and(|item| item.is_dirty(cx))
    })
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    cx.read(|cx| workspace.read(cx).active_pane().clone())
}

fn focus(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&pane.focus_handle(cx), cx));
    cx.run_until_parked();
}

fn pane_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    cx.read(|cx| workspace.read(cx).center_panes().len())
}

fn open_switcher(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> SessionHandle {
    let handle = cx.update(|window, cx| start(workspace, window, cx));
    cx.run_until_parked();
    handle.expect("the switcher opens")
}

/// The switcher that is open now.
fn switcher(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> SessionHandle {
    cx.read(|cx| current(cx, workspace.entity_id()))
        .expect("a switcher is open")
        .handle
}

fn set_input(handle: &SessionHandle, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| handle.set_input(text, window, cx));
    cx.run_until_parked();
}

fn selected_item(handle: &SessionHandle, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.read(|cx| {
        handle.selected(cx).and_then(|value| {
            value
                .downcast_ref::<SwitchTarget>()
                .map(|target| target.item)
        })
    })
}

fn select_next(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
}

fn prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .active_modal::<ReadKeyView>(cx)
            .map(|view| view.read(cx).prompt_text().to_string())
    })
}

fn answer(keys: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

fn rows(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Candidate> {
    cx.read(|cx| candidates(workspace.read(cx), cx))
}

fn titles(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    rows(workspace, cx)
        .iter()
        .map(|row| row.primary.to_string())
        .collect()
}

async fn load(fs: &Arc<FakeFs>, path: &str) -> String {
    fs.load(Path::new(path)).await.expect("the file exists")
}

fn capture_messages(cx: &mut VisualTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::<String>::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.update_global::<MinibufferRouting, _>(|routing, _| {
            routing.set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
        });
    });
    messages
}

#[gpui::test]
async fn kill_on_a_clean_item_closes_it_and_keeps_the_switcher_with_its_input(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let a = open(&workspace, &pane, "a.txt", cx).await;
    let b = open(&workspace, &pane, "b.txt", cx).await;
    let c = open(&workspace, &pane, "c.txt", cx).await;

    let handle = open_switcher(&workspace, cx);
    set_input(&handle, "txt", cx);
    select_next(cx);
    assert_eq!(selected_item(&handle, cx), Some(a.entity_id()));

    // The real key, from the keymap.
    answer("ctrl-k", cx);

    assert_eq!(items(&pane, cx), [b.entity_id(), c.entity_id()]);
    assert!(handle.is_open(), "the switcher stays");
    assert_eq!(cx.read(|cx| handle.input(cx)).as_deref(), Some("txt"));
    assert_eq!(cx.read(|cx| handle.selected_index(cx)), Some(1));
    assert_eq!(prompt(&workspace, cx), None, "a clean item asks nothing");
}

#[gpui::test]
async fn kill_on_the_only_item_closes_it(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    open(&workspace, &pane, "a.txt", cx).await;

    let handle = open_switcher(&workspace, cx);
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();

    assert_eq!(items(&pane, cx), []);
    assert!(handle.is_open());
    assert_eq!(selected_item(&handle, cx), None, "no row is left");
}

#[gpui::test]
async fn kill_on_three_dirty_items_asks_three_questions_and_loses_no_edit(cx: &mut TestAppContext) {
    let (workspace, mut cx, fs) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let a = open(&workspace, &pane, "a.txt", cx).await;
    let b = open(&workspace, &pane, "b.txt", cx).await;
    let c = open(&workspace, &pane, "c.txt", cx).await;
    for editor in [&a, &b, &c] {
        edit(editor, cx);
    }
    let (a, b, c) = (a.entity_id(), b.entity_id(), c.entity_id());

    // The first row is the previous item, b: save.
    let old = open_switcher(&workspace, cx);
    assert_eq!(selected_item(&old, cx), Some(b));
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert!(!old.is_open(), "the switcher closes for the question");
    assert_eq!(
        prompt(&workspace, cx).as_deref(),
        Some("b.txt has unsaved changes: (s)ave, (d)iscard, (c)ancel")
    );
    assert_eq!(
        items(&pane, cx),
        [a, b, c],
        "nothing closes before the answer"
    );
    answer("s", cx);
    assert_eq!(
        load(&fs, "/root/b.txt").await,
        "Xb\n",
        "save writes the file"
    );
    assert_eq!(items(&pane, cx), [a, c]);
    let reopened = switcher(&workspace, cx);
    assert_eq!(selected_item(&reopened, cx), Some(a));

    // Next, a: discard.
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert_eq!(
        prompt(&workspace, cx).as_deref(),
        Some("a.txt has unsaved changes: (s)ave, (d)iscard, (c)ancel")
    );
    answer("d", cx);
    assert_eq!(
        load(&fs, "/root/a.txt").await,
        "a\n",
        "discard writes nothing"
    );
    assert_eq!(items(&pane, cx), [c]);
    let reopened = switcher(&workspace, cx);
    assert_eq!(selected_item(&reopened, cx), Some(c));

    // Last, c: cancel keeps it, dirty.
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert_eq!(
        prompt(&workspace, cx).as_deref(),
        Some("c.txt has unsaved changes: (s)ave, (d)iscard, (c)ancel")
    );
    answer("c", cx);
    assert_eq!(items(&pane, cx), [c]);
    assert!(is_dirty(&pane, c, cx), "the edit is still there");
    assert_eq!(load(&fs, "/root/c.txt").await, "c\n");
    assert!(switcher(&workspace, cx).is_open());
    assert_eq!(prompt(&workspace, cx), None);
}

#[gpui::test]
async fn kill_with_ctrl_g_at_the_question_closes_nothing_and_keeps_the_input(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fs) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let a = open(&workspace, &pane, "a.txt", cx).await;
    open(&workspace, &pane, "b.txt", cx).await;
    open(&workspace, &pane, "c.txt", cx).await;
    edit(&a, cx);

    // The rows are b, a, c: a is the second one.
    let handle = open_switcher(&workspace, cx);
    set_input(&handle, "txt", cx);
    select_next(cx);
    assert_eq!(selected_item(&handle, cx), Some(a.entity_id()));
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert!(prompt(&workspace, cx).is_some());
    answer("ctrl-g", cx);

    assert!(is_dirty(&pane, a.entity_id(), cx));
    assert_eq!(load(&fs, "/root/a.txt").await, "a\n");
    let reopened = switcher(&workspace, cx);
    assert_eq!(cx.read(|cx| reopened.input(cx)).as_deref(), Some("txt"));
    assert_eq!(selected_item(&reopened, cx), Some(a.entity_id()));
    assert_eq!(cx.read(|cx| reopened.selected_index(cx)), Some(1));
}

#[gpui::test]
async fn a_failed_save_keeps_the_item_open_and_dirty_and_echoes_the_error(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let messages = capture_messages(cx);
    let pane = active_pane(&workspace, cx);
    let failing = cx.update(|_, cx| {
        cx.new(|cx| FailingItem {
            focus_handle: cx.focus_handle(),
        })
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item(
            pane.clone(),
            Box::new(failing.clone()),
            None,
            true,
            true,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    let handle = open_switcher(&workspace, cx);
    set_input(&handle, "Fail", cx);
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert!(prompt(&workspace, cx).is_some());
    messages.borrow_mut().clear();
    answer("s", cx);

    let failing = failing.entity_id();
    assert_eq!(items(&pane, cx), [failing], "the item stays open");
    assert!(is_dirty(&pane, failing, cx), "and dirty");
    assert!(
        messages
            .borrow()
            .iter()
            .any(|message| message.contains("Failing.kt") && message.contains("disk full")),
        "the error is echoed: {:?}",
        messages.borrow()
    );
    let reopened = switcher(&workspace, cx);
    assert_eq!(cx.read(|cx| reopened.input(cx)).as_deref(), Some("Fail"));
}

#[gpui::test]
async fn an_item_closed_elsewhere_during_the_question_makes_the_answer_do_nothing(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fs) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let a = open(&workspace, &pane, "a.txt", cx).await;
    let b = open(&workspace, &pane, "b.txt", cx).await;
    edit(&a, cx);

    open_switcher(&workspace, cx);
    cx.dispatch_action(KillBuffer);
    cx.run_until_parked();
    assert!(prompt(&workspace, cx).is_some());
    // Closed from elsewhere while the question is open.
    let a = a.entity_id();
    close(&pane, a, cx);
    assert_eq!(items(&pane, cx), [b.entity_id()]);

    answer("s", cx);

    assert_eq!(
        items(&pane, cx),
        [b.entity_id()],
        "the answer closes nothing"
    );
    assert_eq!(
        load(&fs, "/root/a.txt").await,
        "a\n",
        "and saves nothing: the item was discarded elsewhere"
    );
    assert_eq!(prompt(&workspace, cx), None);
    let reopened = switcher(&workspace, cx);
    assert!(reopened.is_open());
    assert_eq!(selected_item(&reopened, cx), Some(b.entity_id()));
}

/// Opens a.txt, b.txt and c.txt, and closes a.txt, which stays on the navigation history.
async fn with_a_closed_file(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    cx: &mut VisualTestContext,
) -> (EntityId, EntityId) {
    let a = open(workspace, pane, "a.txt", cx).await;
    record_in_history(pane, cx);
    let b = open(workspace, pane, "b.txt", cx).await;
    record_in_history(pane, cx);
    open(workspace, pane, "c.txt", cx).await;
    close(pane, a.entity_id(), cx);
    (a.entity_id(), b.entity_id())
}

#[gpui::test]
async fn recently_closed_rows_come_after_open_ones_dimmed_and_the_setting_hides_them(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    with_a_closed_file(&workspace, &pane, cx).await;

    let rows = rows(&workspace, cx);
    let names: Vec<_> = rows.iter().map(|row| row.primary.as_ref()).collect();
    assert_eq!(
        names,
        ["b.txt", "c.txt", "a.txt"],
        "open items, then closed"
    );
    assert_eq!(
        rows.iter().map(|row| row.dimmed).collect::<Vec<_>>(),
        [false, false, true]
    );
    let closed = rows.last().expect("a closed row");
    assert!(matches!(
        closed.value.downcast_ref::<FileTarget>(),
        Some(FileTarget::Project(path)) if path.path.as_unix_str() == "a.txt"
    ));

    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .navigation
                    .get_or_insert_default()
                    .include_recent_closed = Some(false);
            });
        });
    });
    assert_eq!(
        titles(&workspace, cx),
        ["b.txt", "c.txt"],
        "hidden when off"
    );
}

#[gpui::test]
async fn recently_closed_rows_are_at_most_fifty(cx: &mut TestAppContext) {
    let (workspace, mut cx, fs) = setup(cx).await;
    let cx = &mut cx;
    fs.create_dir(Path::new("/root/many"))
        .await
        .expect("the directory is created");
    for ix in 0..55 {
        fs.insert_file(format!("/root/many/f{ix:02}.txt"), b"x\n".to_vec())
            .await;
    }
    cx.run_until_parked();
    let pane = active_pane(&workspace, cx);
    for ix in 0..55 {
        let editor = open(&workspace, &pane, &format!("many/f{ix:02}.txt"), cx).await;
        record_in_history(&pane, cx);
        close(&pane, editor.entity_id(), cx);
    }

    let closed = rows(&workspace, cx).iter().filter(|row| row.dimmed).count();
    assert_eq!(closed, 50);
}

#[gpui::test]
async fn ret_on_a_recently_closed_row_reopens_the_file_and_the_row_previews(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    with_a_closed_file(&workspace, &pane, cx).await;

    let handle = open_switcher(&workspace, cx);
    select_next(cx);
    select_next(cx);
    assert!(cx.read(|cx| {
        handle
            .selected(cx)
            .is_some_and(|value| value.downcast_ref::<FileTarget>().is_some())
    }));
    assert!(
        cx.read(|cx| pane.read(cx).preview().is_some()),
        "the closed file previews in its pane"
    );
    let before = items(&pane, cx);
    assert_eq!(before.len(), 2, "previewing adds no item");

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();

    let titles = cx.read(|cx| {
        pane.read(cx)
            .items()
            .map(|item| item.tab_content_text(0, cx).to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(titles, ["b.txt", "c.txt", "a.txt"]);
    assert_eq!(
        cx.read(|cx| pane
            .read(cx)
            .active_item()
            .map(|item| item.tab_content_text(0, cx))),
        Some("a.txt".into())
    );
    assert!(cx.read(|cx| pane.read(cx).preview().is_none()));
}

/// Two panes: `source` with `keep` tabs and the item to move last, and the pane that is not
/// active. Returns the panes and the item.
async fn two_panes_with_an_item_to_move(
    workspace: &Entity<Workspace>,
    keep: &[&str],
    cx: &mut VisualTestContext,
) -> (Vec<Entity<Pane>>, EntityId) {
    let panes = cx.update(|window, cx| split_center(workspace, 2, window, cx));
    open(workspace, &panes[1], "c.txt", cx).await;
    for name in keep {
        open(workspace, &panes[0], name, cx).await;
    }
    let moved = open(workspace, &panes[0], "moved.txt", cx).await;
    focus(&panes[0], cx);
    (panes, moved.entity_id())
}

/// Opens the switcher on `moved.txt`, opens the M-o menu and answers it with `keys`.
fn move_through_menu(workspace: &Entity<Workspace>, keys: &str, cx: &mut VisualTestContext) {
    let handle = open_switcher(workspace, cx);
    set_input(&handle, "moved", cx);
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    answer(keys, cx);
}

#[gpui::test]
async fn m_o_other_pane_moves_the_item_and_keeps_every_tab_of_the_source(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let (panes, moved) =
        two_panes_with_an_item_to_move(&workspace, &["keep1.txt", "keep2.txt"], cx).await;
    let kept = items(&panes[0], cx)[..2].to_vec();

    move_through_menu(&workspace, "o", cx);

    assert_eq!(items(&panes[0], cx), kept, "every other tab stays");
    assert!(items(&panes[1], cx).contains(&moved));
    assert_eq!(pane_count(&workspace, cx), 2);
    assert_eq!(active_pane(&workspace, cx), panes[1]);
}

#[gpui::test]
async fn m_o_split_right_moves_the_item_into_a_new_pane_keeping_the_source_tabs(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let (panes, moved) =
        two_panes_with_an_item_to_move(&workspace, &["keep1.txt", "keep2.txt"], cx).await;
    let kept = items(&panes[0], cx)[..2].to_vec();

    move_through_menu(&workspace, "r", cx);

    assert_eq!(items(&panes[0], cx), kept);
    assert_eq!(pane_count(&workspace, cx), 3);
    let created = active_pane(&workspace, cx);
    assert!(!panes.contains(&created));
    assert_eq!(items(&created, cx), [moved]);
}

#[gpui::test]
async fn m_o_split_down_moves_the_item_and_keeps_a_source_that_it_empties(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let (panes, moved) = two_panes_with_an_item_to_move(&workspace, &[], cx).await;
    assert_eq!(items(&panes[0], cx), [moved]);

    move_through_menu(&workspace, "d", cx);

    assert_eq!(items(&panes[0], cx), [], "the source stays, empty");
    assert_eq!(pane_count(&workspace, cx), 3);
    let created = active_pane(&workspace, cx);
    assert!(!panes.contains(&created));
    assert_eq!(items(&created, cx), [moved]);
}

#[gpui::test]
async fn m_o_pane_by_letter_moves_the_item_into_the_picked_pane(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 3, window, cx));
    open(&workspace, &panes[2], "c.txt", cx).await;
    open(&workspace, &panes[0], "keep1.txt", cx).await;
    let moved = open(&workspace, &panes[0], "moved.txt", cx)
        .await
        .entity_id();
    focus(&panes[0], cx);
    let kept = items(&panes[0], cx)[..1].to_vec();

    move_through_menu(&workspace, "p", cx);
    assert!(
        items(&panes[2], cx).len() == 1,
        "nothing moves before a letter"
    );
    answer("d", cx);

    assert_eq!(items(&panes[0], cx), kept);
    assert!(items(&panes[2], cx).contains(&moved));
    assert!(items(&panes[1], cx).is_empty());
    assert_eq!(pane_count(&workspace, cx), 3);
}

#[gpui::test]
async fn m_o_on_a_recently_closed_row_opens_the_file_in_the_chosen_pane(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    let a = open(&workspace, &panes[0], "a.txt", cx).await;
    record_in_history(&panes[0], cx);
    open(&workspace, &panes[0], "b.txt", cx).await;
    close(&panes[0], a.entity_id(), cx);
    focus(&panes[0], cx);

    let handle = open_switcher(&workspace, cx);
    set_input(&handle, "a.txt", cx);
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    answer("o", cx);

    let titles = cx.read(|cx| {
        panes[1]
            .read(cx)
            .items()
            .map(|item| item.tab_content_text(0, cx).to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(titles, ["a.txt"]);
    assert_eq!(
        items(&panes[0], cx).len(),
        1,
        "the source pane is as it was"
    );
}
