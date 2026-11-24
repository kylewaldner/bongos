use bongos_minibuffer::minibuffer_for_window;
use editor::{Editor, MultiBuffer, PathKey, actions::OpenExcerpts};
use gpui::{AppContext as _, Entity, EntityId, TestAppContext, VisualTestContext, WindowHandle};
use language::{Capability, Point};
use project::{Fs as _, Project, ProjectPath, WorktreeId};
use workspace::{
    AppState, MultiWorkspace, OpenTarget, Pane, Workspace, WorkspaceHandle as _, dock::DockPosition,
};

use crate::actions::ToggleWindowDedication;
use crate::test_support::{add_editor, add_panel, add_terminal, new_dock_pane, split_center};

/// A workspace in a multi-workspace window on a project of three 40-line files, `one.txt`,
/// `two.txt` and `three.txt`, none open yet.
async fn setup(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, [ProjectPath; 3], VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        bongos_emacs::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        params
    });
    let fake = params.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    for name in ["one", "two", "three"] {
        let text = (1..=40)
            .map(|ix| format!("{name} {ix}\n"))
            .collect::<String>();
        fake.insert_file(format!("/root/{name}.txt"), text.into_bytes())
            .await;
    }
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
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

/// `count` center panes, each showing a new editor.
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
        (panes, editors)
    });
    cx.run_until_parked();
    result
}

/// Focuses the panes in the given order, so the last one is the active pane and every pane's
/// activation is newer than the one before it.
fn focus_in_order(panes: &[&Entity<Pane>], cx: &mut VisualTestContext) {
    for pane in panes {
        cx.update(|window, cx| {
            let handle = pane
                .read(cx)
                .active_item()
                .expect("an item")
                .item_focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
    }
}

fn dedicate(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|_, cx| pane.update(cx, |pane, cx| pane.set_dedicated(true, cx)));
}

fn item_ids(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<EntityId> {
    cx.update(|_, cx| pane.read(cx).items().map(|item| item.item_id()).collect())
}

fn active_path(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<ProjectPath> {
    cx.update(|_, cx| {
        pane.read(cx)
            .active_item()
            .and_then(|item| item.project_path(cx))
    })
}

fn center_panes(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Entity<Pane>> {
    cx.update(|_, cx| workspace.read(cx).center_panes())
}

/// Opens `path` as the project panel does: no pane named, preview allowed.
async fn open_preview(
    workspace: &Entity<Workspace>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path_preview(path.clone(), None, true, true, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
}

/// The echo area's log, newest first.
fn echoed(cx: &mut VisualTestContext) -> Vec<String> {
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    })
}

fn toggle(cx: &mut VisualTestContext) {
    cx.dispatch_action(ToggleWindowDedication);
    cx.run_until_parked();
}

fn is_dedicated(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| pane.read(cx).is_dedicated())
}

#[gpui::test]
async fn toggling_marks_and_unmarks_the_focused_window(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 2, cx);
    focus_in_order(&[&panes[0], &panes[1]], cx);
    toggle(cx);
    assert!(is_dedicated(&panes[1], cx));
    assert!(!is_dedicated(&panes[0], cx));
    assert_eq!(echoed(cx), vec!["Window dedicated".to_string()]);
    toggle(cx);
    assert!(!is_dedicated(&panes[1], cx));
    assert_eq!(
        echoed(cx),
        vec![
            "Window no longer dedicated".to_string(),
            "Window dedicated".to_string()
        ]
    );

    // A dock's pane is dedicated the same way.
    let (dock_pane, panel) = cx.update(|window, cx| {
        let dock_pane = new_dock_pane(&workspace, window, cx);
        add_terminal(&workspace, &dock_pane, window, cx);
        let panel = add_panel(
            &workspace,
            DockPosition::Bottom,
            10,
            Some(dock_pane.clone()),
            true,
            window,
            cx,
        );
        (dock_pane, panel)
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let handle = panel.read(cx).focus_handle.clone();
        window.focus(&handle, cx)
    });
    cx.run_until_parked();
    toggle(cx);
    assert!(is_dedicated(&dock_pane, cx));
    assert!(!is_dedicated(&panes[0], cx));
    assert!(!is_dedicated(&panes[1], cx));
    assert_eq!(echoed(cx)[0], "Window dedicated");
}

#[gpui::test]
async fn a_project_panel_open_skips_the_dedicated_window(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 3, cx);
    focus_in_order(&[&panes[2], &panes[0], &panes[1]], cx);
    dedicate(&panes[1], cx);
    let before = [item_ids(&panes[1], cx), item_ids(&panes[2], cx)];
    open_preview(&workspace, &paths[0], cx).await;
    assert_eq!(active_path(&panes[0], cx), Some(paths[0].clone()));
    assert_eq!(item_ids(&panes[1], cx), before[0]);
    assert_eq!(item_ids(&panes[2], cx), before[1]);
    assert_eq!(center_panes(&workspace, cx), panes);
}

#[gpui::test]
async fn a_file_finder_open_skips_the_dedicated_window(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 3, cx);
    focus_in_order(&[&panes[0], &panes[2], &panes[1]], cx);
    dedicate(&panes[1], cx);
    let before = [item_ids(&panes[0], cx), item_ids(&panes[1], cx)];
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(paths[1].clone(), None, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(active_path(&panes[2], cx), Some(paths[1].clone()));
    assert_eq!(item_ids(&panes[0], cx), before[0]);
    assert_eq!(item_ids(&panes[1], cx), before[1]);
    assert_eq!(center_panes(&workspace, cx), panes);
}

#[gpui::test]
async fn a_search_result_open_skips_the_dedicated_window(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 3, cx);
    let project = cx.update(|_, cx| workspace.read(cx).project().clone());
    let mut buffers = Vec::new();
    for path in &paths[..2] {
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
            .await
            .unwrap();
        buffers.push(buffer);
    }
    // Search results: a multi-buffer of one excerpt from each of two files, shown in the
    // middle pane.
    let results = cx.update(|window, cx| {
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::new(Capability::ReadWrite);
            for (ix, buffer) in buffers.iter().enumerate() {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(ix as u64),
                    buffer.clone(),
                    [Point::new(0, 0)..Point::new(2, 0)],
                    0,
                    cx,
                );
            }
            multi_buffer
        });
        let results =
            cx.new(|cx| Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx));
        workspace.update(cx, |workspace, cx| {
            workspace.add_item(
                panes[1].clone(),
                Box::new(results.clone()),
                None,
                true,
                true,
                window,
                cx,
            )
        });
        results
    });
    cx.run_until_parked();
    focus_in_order(&[&panes[2], &panes[0], &panes[1]], cx);
    dedicate(&panes[1], cx);
    let before = [item_ids(&panes[1], cx), item_ids(&panes[2], cx)];
    results.update_in(cx, |results, window, cx| {
        results.open_excerpts(&OpenExcerpts, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(active_path(&panes[0], cx), Some(paths[0].clone()));
    assert_eq!(item_ids(&panes[1], cx), before[0]);
    assert_eq!(item_ids(&panes[2], cx), before[1]);
    assert_eq!(center_panes(&workspace, cx), panes);
}

#[gpui::test]
async fn the_dedicated_windows_preview_tab_is_kept(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 2, cx);
    focus_in_order(&[&panes[0], &panes[1]], cx);
    open_preview(&workspace, &paths[0], cx).await;
    let preview = cx.update(|_, cx| panes[1].read(cx).preview_item_id());
    assert!(preview.is_some());
    dedicate(&panes[1], cx);
    let before = item_ids(&panes[1], cx);
    open_preview(&workspace, &paths[1], cx).await;
    assert_eq!(item_ids(&panes[1], cx), before);
    assert_eq!(
        cx.update(|_, cx| panes[1].read(cx).preview_item_id()),
        preview
    );
    assert_eq!(active_path(&panes[1], cx), Some(paths[0].clone()));
    assert_eq!(active_path(&panes[0], cx), Some(paths[1].clone()));
}

#[gpui::test]
async fn with_every_window_dedicated_an_open_splits_a_new_one(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 2, cx);
    focus_in_order(&[&panes[1], &panes[0]], cx);
    dedicate(&panes[0], cx);
    dedicate(&panes[1], cx);
    let before = [item_ids(&panes[0], cx), item_ids(&panes[1], cx)];

    // An open whose load fails splits nothing.
    let missing = ProjectPath {
        worktree_id: WorktreeId::from_usize(usize::MAX),
        path: paths[0].path.clone(),
    };
    let failed = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(missing, None, true, window, cx)
        })
        .await;
    cx.run_until_parked();
    assert!(failed.is_err());
    assert_eq!(center_panes(&workspace, cx), panes);

    // Two opens at once: the first splits a pane right of the active one, the second uses it.
    let (first, second) = workspace.update_in(cx, |workspace, window, cx| {
        (
            workspace.open_path(paths[1].clone(), None, true, window, cx),
            workspace.open_path(paths[2].clone(), None, true, window, cx),
        )
    });
    first.await.unwrap();
    second.await.unwrap();
    cx.run_until_parked();
    let after = center_panes(&workspace, cx);
    assert_eq!(after.len(), 3);
    assert_eq!(after[0], panes[0]);
    assert_eq!(after[2], panes[1]);
    let new_pane = after[1].clone();
    assert!(!is_dedicated(&new_pane, cx));
    let opened = cx.update(|_, cx| {
        new_pane
            .read(cx)
            .items()
            .filter_map(|item| item.project_path(cx))
            .collect::<Vec<_>>()
    });
    assert_eq!(opened.len(), 2);
    assert!(opened.contains(&paths[1]) && opened.contains(&paths[2]));
    assert_eq!(item_ids(&panes[0], cx), before[0]);
    assert_eq!(item_ids(&panes[1], cx), before[1]);
    assert!(is_dedicated(&panes[0], cx));
    assert!(is_dedicated(&panes[1], cx));
}

#[gpui::test]
async fn open_target_names_the_split_that_pane_for_open_makes(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 2, cx);
    dedicate(&panes[0], cx);
    dedicate(&panes[1], cx);

    let target = cx.update(|_, cx| workspace.read(cx).open_target(&panes[1], cx));
    assert_eq!(target, OpenTarget::SplitRightOf(panes[1].clone()));
    assert_eq!(center_panes(&workspace, cx), panes);

    let opened = workspace.update_in(cx, |workspace, window, cx| {
        workspace.pane_for_open(panes[1].clone(), window, cx)
    });
    let after = center_panes(&workspace, cx);
    assert_eq!(after.len(), 3);
    assert_eq!(after[2], opened);
    assert!(!panes.contains(&opened));

    // With a pane that may take the open, the target is that pane and nothing splits.
    let target = cx.update(|_, cx| workspace.read(cx).open_target(&panes[1], cx));
    assert_eq!(target, OpenTarget::Pane(opened));
    assert_eq!(center_panes(&workspace, cx).len(), 3);
}

#[gpui::test]
async fn without_a_dedicated_window_opens_are_unchanged(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 3, cx);
    focus_in_order(&[&panes[2], &panes[0], &panes[1]], cx);
    let before = [item_ids(&panes[0], cx), item_ids(&panes[2], cx)];
    open_preview(&workspace, &paths[0], cx).await;
    assert_eq!(active_path(&panes[1], cx), Some(paths[0].clone()));
    assert!(
        cx.update(|_, cx| panes[1].read(cx).preview_item_id())
            .is_some()
    );
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(paths[1].clone(), None, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(active_path(&panes[1], cx), Some(paths[1].clone()));
    assert_eq!(item_ids(&panes[0], cx), before[0]);
    assert_eq!(item_ids(&panes[2], cx), before[1]);
    assert_eq!(center_panes(&workspace, cx), panes);
}
