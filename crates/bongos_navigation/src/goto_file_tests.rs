//! Tests of the go to file picker: its rows and their order, the typed paths, the preview in the
//! resolved pane and what confirming opens.

use std::{path::PathBuf, sync::Arc};

use editor::Editor;

use bongos_minibuffer::SessionHandle;
use bongos_minibuffer_model::Cancel;
use bongos_window::test_support::{add_editor, split_center};
use gpui::{
    App, BorrowAppContext as _, Entity, EntityId, Focusable as _, TestAppContext,
    VisualTestContext, WindowHandle,
};
use project::{Fs as _, Project, ProjectPath};
use serde_json::json;
use settings::SettingsStore;
use util::paths::home_dir;
use workspace::{
    AppState, MultiWorkspace, Pane, Workspace, WorkspaceHandle as _, item::WeakItemHandle,
};
use zed_actions::minibuffer;

use crate::goto_file::{HOST, PathIndex, start};
use crate::goto_file_open::FileTarget;
use crate::target::PaneChoice;
use crate::test_support::{WorldSnapshot, world_snapshot};

/// A workspace on `/root` holding `tree`, and the `extra` files outside it.
async fn setup(
    cx: &mut TestAppContext,
    tree: serde_json::Value,
    extra: &[(PathBuf, &str)],
) -> (Entity<Workspace>, VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
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
    let fake = params.fs.as_fake();
    fake.insert_tree("/root", tree).await;
    for (path, content) in extra {
        let parent = path.parent().expect("a file has a directory");
        fake.create_dir(parent)
            .await
            .expect("the directory is created");
        fake.insert_file(path, content.as_bytes().to_vec()).await;
    }
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
    (workspace, cx)
}

fn tree() -> serde_json::Value {
    json!({
        ".git": {},
        ".gitignore": "build/\n*.log\n",
        "src": {
            "Main.kt": "fun main() {}\n",
            "Alpha.kt": "alpha\n",
            "Beta.kt": "beta\n",
        },
        "build": { "out.class": "" },
        "debug.log": "",
    })
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

async fn open_file(workspace: &Entity<Workspace>, rel: &str, cx: &mut VisualTestContext) {
    let path = project_path(workspace, rel, cx);
    let task = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path(path, None, true, window, cx)
    });
    task.await.expect("the file opens");
    cx.run_until_parked();
}

/// Puts the active item of the active pane on that pane's navigation history, as leaving it
/// with a jump would.
fn record_in_history(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    cx.update(|_, cx| {
        let editor = pane
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("the active item is an editor");
        let item: Arc<dyn WeakItemHandle + Send + Sync> = Arc::new(editor.downgrade());
        pane.update(cx, |pane, cx| {
            pane.nav_history_mut()
                .push::<()>(None, item, false, None, cx)
        });
    });
}

fn index(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> PathIndex {
    cx.read(|cx| PathIndex::new(workspace.read(cx), cx))
}

fn open_picker(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> SessionHandle {
    let handle = cx.update(|window, cx| start(workspace, PaneChoice::Default, window, cx));
    cx.run_until_parked();
    handle.expect("the picker opens")
}

fn type_input(handle: &SessionHandle, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| handle.set_input(text, window, cx));
    cx.run_until_parked();
}

fn selected(handle: &SessionHandle, cx: &mut VisualTestContext) -> Option<String> {
    let value = cx.read(|cx| handle.selected(cx))?;
    match value.downcast_ref::<FileTarget>()? {
        FileTarget::Project(path) => Some(path.path.as_unix_str().to_string()),
        FileTarget::Abs(path) => Some(path.display().to_string()),
    }
}

fn select_next(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
}

/// The first `count` rows, in order, found by walking the selection down the list.
fn listed(handle: &SessionHandle, count: usize, cx: &mut VisualTestContext) -> Vec<String> {
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.extend(selected(handle, cx));
        select_next(cx);
    }
    rows
}

fn confirm(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

fn center_panes(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    let panes = cx.update(|window, cx| {
        let panes = split_center(workspace, count, window, cx);
        for pane in &panes {
            add_editor(workspace, pane, window, cx);
        }
        panes
    });
    cx.run_until_parked();
    panes
}

fn activate(workspace: &Entity<Workspace>, pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&pane.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(
        &workspace.read_with(cx, |w, _| w.active_pane().clone()),
        pane
    );
}

fn dedicate(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    pane.update(cx, |pane, cx| pane.set_dedicated(true, cx));
}

fn pane_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.center_panes().len())
}

fn slot(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.read(|cx| pane.read(cx).preview().map(|view| view.entity_id()))
}

fn active_path(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| {
        let item = pane.read(cx).active_item()?;
        item.project_path(cx)
            .map(|path| path.path.as_unix_str().to_string())
    })
}

/// The world as a picker must leave it; a pane that takes the focus back when the picker closes
/// stamps its active item again, so the stamps are dropped.
fn snapshot(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> WorldSnapshot {
    let mut snapshot = cx.update(|_, cx: &mut App| world_snapshot(workspace, cx));
    for pane in &mut snapshot.panes {
        for (_, stamp) in &mut pane.activation_history {
            *stamp = 0;
        }
    }
    snapshot
}

#[gpui::test]
async fn goto_file_empty_input_lists_recent_files_first(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, tree(), &[]).await;
    let cx = &mut cx;
    open_file(&workspace, "src/Beta.kt", cx).await;
    record_in_history(&workspace, cx);
    open_file(&workspace, "src/Main.kt", cx).await;

    let listing = index(&workspace, cx).listing().join(",");
    assert!(
        listing.starts_with("src/Main.kt,src/Beta.kt,.gitignore,src/Alpha.kt"),
        "recent files first, then path order: {listing}"
    );
    let handle = open_picker(&workspace, cx);
    assert_eq!(
        listed(&handle, 4, cx),
        ["src/Main.kt", "src/Beta.kt", ".gitignore", "src/Alpha.kt"]
    );
}

#[gpui::test]
async fn goto_file_leaves_out_ignored_files(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, tree(), &[]).await;
    let cx = &mut cx;

    let index = index(&workspace, cx);
    let listing = index.listing();
    assert_eq!(
        listing,
        [".gitignore", "src/Alpha.kt", "src/Beta.kt", "src/Main.kt"]
    );
    let handle = open_picker(&workspace, cx);
    type_input(&handle, "out", cx);
    assert_eq!(selected(&handle, cx), None);
    type_input(&handle, "debug", cx);
    assert_eq!(selected(&handle, cx), None);
}

#[gpui::test]
async fn goto_file_inv_svc_impl_finds_the_file_with_name_highlighted_and_directory_muted(
    cx: &mut TestAppContext,
) {
    let tree = json!({
        "billing": {
            "InvoiceServiceImpl.kt": "",
            "Ledger.kt": "",
        },
        "README.md": "",
    });
    let (workspace, mut cx) = setup(cx, tree, &[]).await;
    let cx = &mut cx;

    let rows = index(&workspace, cx)
        .rows("inv svc impl", &Cancel::new())
        .expect("the pass is not cancelled");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.primary.as_ref(), "InvoiceServiceImpl.kt");
    assert_eq!(row.secondary.as_deref(), Some("billing"));
    // `Inv` at the start, `s`, `v` and `c` scattered over `Service`, and `Impl`.
    assert_eq!(row.highlights, [0..3, 7..8, 10..11, 12..13, 14..18]);
    assert!(!row.dimmed);

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "inv svc impl", cx);
    assert_eq!(
        selected(&handle, cx).as_deref(),
        Some("billing/InvoiceServiceImpl.kt")
    );
}

#[gpui::test]
async fn goto_file_contiguous_segments_rank_above_scattered_characters(cx: &mut TestAppContext) {
    let tree = json!({
        "src": { "stu": { "ff": { "File.kt": "" } } },
        "stuff": { "File.kt": "" },
        "x": { "stuff": { "File.kt": "" } },
    });
    let (workspace, mut cx) = setup(cx, tree, &[]).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, cx);

    type_input(&handle, "stuff/File", cx);
    let mut rows = listed(&handle, 3, cx);
    assert_eq!(rows.pop().as_deref(), Some("src/stu/ff/File.kt"));
    rows.sort();
    assert_eq!(rows, ["stuff/File.kt", "x/stuff/File.kt"]);

    type_input(&handle, "x/stuff/File.kt", cx);
    assert_eq!(selected(&handle, cx).as_deref(), Some("x/stuff/File.kt"));
    type_input(&handle, "src/stu/ff/File.kt", cx);
    assert_eq!(selected(&handle, cx).as_deref(), Some("src/stu/ff/File.kt"));
}

#[gpui::test]
async fn goto_file_absolute_and_home_paths_open_directly(cx: &mut TestAppContext) {
    let home_file = home_dir().join("bongos_goto_file_note.txt");
    let extra = [
        (PathBuf::from("/elsewhere/outside.txt"), "outside\n"),
        (home_file.clone(), "home\n"),
    ];
    let (workspace, mut cx) = setup(cx, tree(), &extra).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, cx);

    type_input(&handle, "/root/src/Main.kt", cx);
    assert_eq!(selected(&handle, cx).as_deref(), Some("src/Main.kt"));
    type_input(&handle, "/root/src/Missing.kt", cx);
    assert_eq!(selected(&handle, cx), None);
    type_input(&handle, "/root/src", cx);
    assert_eq!(selected(&handle, cx), None, "a directory is not listed");
    type_input(&handle, "~/bongos_goto_file_note.txt", cx);
    assert_eq!(selected(&handle, cx), Some(home_file.display().to_string()));

    type_input(&handle, "/elsewhere/outside.txt", cx);
    assert_eq!(
        selected(&handle, cx).as_deref(),
        Some("/elsewhere/outside.txt")
    );
    confirm(cx);
    let opened = cx.read(|cx| {
        let workspace = workspace.read(cx);
        let item = workspace.active_pane().read(cx).active_item()?;
        let path = item.project_path(cx)?;
        workspace.project().read(cx).absolute_path(&path, cx)
    });
    assert_eq!(opened, Some(PathBuf::from("/elsewhere/outside.txt")));
}

#[gpui::test]
async fn goto_file_preview_shows_in_the_resolved_pane_and_cancel_leaves_the_world_as_it_was(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, tree(), &[]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[1], cx);
    let before = snapshot(&workspace, cx);

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "Beta", cx);
    assert_eq!(selected(&handle, cx).as_deref(), Some("src/Beta.kt"));
    assert!(
        slot(&panes[1], cx).is_some(),
        "the preview is in the active pane"
    );
    assert_eq!(slot(&panes[0], cx), None);
    assert_eq!(snapshot(&workspace, cx).without_previews(), before);
    assert_eq!(pane_count(&workspace, cx), 2);

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(slot(&panes[1], cx), None);
    assert_eq!(snapshot(&workspace, cx), before);
}

#[gpui::test]
async fn goto_file_confirm_opens_in_the_resolved_pane_skipping_a_dedicated_one(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, tree(), &[]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);
    dedicate(&panes[0], cx);
    let items_before = cx.read(|cx| panes[0].read(cx).items_len());

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "Beta", cx);
    assert!(
        slot(&panes[1], cx).is_some(),
        "the preview skips the dedicated pane"
    );
    assert_eq!(slot(&panes[0], cx), None);
    confirm(cx);

    assert_eq!(active_path(&panes[1], cx).as_deref(), Some("src/Beta.kt"));
    assert_eq!(cx.read(|cx| panes[0].read(cx).items_len()), items_before);
    assert_eq!(slot(&panes[1], cx), None, "confirming clears the preview");
    assert_eq!(pane_count(&workspace, cx), 2);
}

#[gpui::test]
async fn goto_file_with_every_pane_dedicated_previews_without_a_pane_and_splits_once(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, tree(), &[]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);
    for pane in &panes {
        dedicate(pane, cx);
    }

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "Beta", cx);
    assert!(
        slot(&panes[0], cx).is_some(),
        "the preview shows in the pane to be split"
    );
    assert_eq!(pane_count(&workspace, cx), 2, "previewing adds no pane");
    confirm(cx);

    assert_eq!(pane_count(&workspace, cx), 3, "confirming splits once");
    let all = workspace.read_with(cx, |workspace, _| workspace.center_panes());
    let new_pane = all
        .iter()
        .find(|pane| !panes.contains(pane))
        .expect("a new pane")
        .clone();
    assert_eq!(active_path(&new_pane, cx).as_deref(), Some("src/Beta.kt"));
    assert_eq!(slot(&panes[0], cx), None);
}
