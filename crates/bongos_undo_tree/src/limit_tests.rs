//! Tests of the undo tree's size: the node limit a buffer gets, a settings change reaching open
//! buffers, and the history of a buffer being dropped when its last full editor closes.

use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AppContext as _, BorrowAppContext as _, Context, Empty, Entity, IntoElement, Render,
    TestAppContext, VisualTestContext, Window,
};
use language::Buffer;
use project::Project;
use settings::SettingsStore;
use workspace::AppState;

/// The window's view; it owns the editors, so that dropping one releases it.
#[derive(Default)]
struct Root {
    editors: Vec<Option<Entity<Editor>>>,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn set_limit(limit: usize, cx: &mut App) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.undo_tree.get_or_insert_default().node_limit = Some(limit);
        });
    });
}

struct Setup {
    root: Entity<Root>,
    project: Entity<Project>,
    buffer: Entity<Buffer>,
}

/// The undo tree set up with `limit`, a window, a project with a file `notes.txt` ("one") and its
/// open buffer.
async fn setup(cx: &mut TestAppContext, limit: usize) -> (Setup, &mut VisualTestContext) {
    let app_state: Arc<AppState> = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    cx.update(|cx| set_limit(limit, cx));
    app_state
        .fs
        .as_fake()
        .insert_tree("/dir", serde_json::json!({ "notes.txt": "one" }))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    let (root, cx) = cx.add_window_view(|_, _| Root::default());
    let buffer = open_buffer(&project, cx).await;
    (
        Setup {
            root,
            project,
            buffer,
        },
        cx,
    )
}

async fn open_buffer(project: &Entity<Project>, cx: &mut VisualTestContext) -> Entity<Buffer> {
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/notes.txt", cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    buffer
}

/// Opens a full editor on the buffer; returns its index for [`close_editor`].
fn open_editor(setup: &Setup, cx: &mut VisualTestContext) -> usize {
    let buffer = setup.buffer.clone();
    let project = setup.project.clone();
    setup.root.update_in(cx, |root, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
        root.editors.push(Some(editor));
        root.editors.len() - 1
    })
}

fn close_editor(setup: &Setup, index: usize, cx: &mut VisualTestContext) {
    setup.root.update(cx, |root, _| root.editors[index] = None);
    cx.run_until_parked();
}

fn type_text(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) {
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
    cx.run_until_parked();
}

fn tree_len(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) -> usize {
    buffer.read_with(cx, |buffer, _| buffer.undo_tree().len())
}

fn limit(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) -> usize {
    buffer.read_with(cx, |buffer, _| buffer.undo_tree().limit())
}

#[gpui::test]
async fn new_buffer_gets_the_configured_limit(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, 321).await;
    let created = cx.new(|cx| Buffer::local("", cx));
    cx.run_until_parked();
    assert_eq!(limit(&created, cx), 321);
    // A buffer loaded from disk gets the limit from the editor that opens it.
    open_editor(&setup, cx);
    assert_eq!(limit(&setup.buffer, cx), 321);
}

#[gpui::test]
async fn settings_change_reaches_open_buffers(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, 300).await;
    let created = cx.new(|cx| Buffer::local("", cx));
    open_editor(&setup, cx);
    cx.run_until_parked();
    cx.update(|_, cx| set_limit(450, cx));
    cx.run_until_parked();
    assert_eq!(limit(&created, cx), 450);
    assert_eq!(limit(&setup.buffer, cx), 450);
}

#[gpui::test]
async fn closing_the_last_editor_resets_a_kept_buffer(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, 300).await;
    let editor = open_editor(&setup, cx);
    type_text(&setup.buffer, cx);
    assert_eq!(tree_len(&setup.buffer, cx), 2);

    close_editor(&setup, editor, cx);

    assert_eq!(tree_len(&setup.buffer, cx), 1);
    assert_eq!(
        setup.buffer.read_with(cx, |buffer, _| buffer.text()),
        "xone"
    );
    assert_eq!(setup.buffer.update(cx, |buffer, cx| buffer.undo(cx)), None);
}

#[gpui::test]
async fn another_editor_keeps_the_tree(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, 300).await;
    let first = open_editor(&setup, cx);
    let second = open_editor(&setup, cx);
    type_text(&setup.buffer, cx);

    close_editor(&setup, first, cx);
    assert_eq!(tree_len(&setup.buffer, cx), 2);

    close_editor(&setup, second, cx);
    assert_eq!(tree_len(&setup.buffer, cx), 1);
}

#[gpui::test]
async fn reopened_file_has_a_fresh_tree(cx: &mut TestAppContext) {
    let (
        Setup {
            root,
            project,
            buffer,
        },
        cx,
    ) = setup(cx, 300).await;
    let setup_again = Setup {
        root: root.clone(),
        project: project.clone(),
        buffer: buffer.clone(),
    };
    let editor = open_editor(&setup_again, cx);
    drop(setup_again);
    type_text(&buffer, cx);
    assert_eq!(tree_len(&buffer, cx), 2);
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();

    root.update(cx, |root, _| root.editors[editor] = None);
    drop(buffer);
    cx.run_until_parked();

    let buffer = open_buffer(&project, cx).await;
    assert_eq!(tree_len(&buffer, cx), 1);
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "xone");
}
