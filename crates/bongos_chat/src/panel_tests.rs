//! The Chats panel and its session view in a real window (`VisualTestContext`): the panes of each
//! layout, the prompts pane toggled without touching the prompts buffer or the terminal, the
//! layout in the record, the panel following `chats.enabled`, and the actions dispatched by name.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_chat_model::{Agent, Layout, read_record, record_path, write_record};
use editor::Editor;
use fs::RealFs;
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::Project;
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Workspace};

use crate::runtime_tests::{Harness, STARTUP, harness, set_chat_settings, wait_for};
use crate::{ChatPanel, ChatRuntime, ChatToast, PROMPT_PANE_OFF, SessionView, init_panel};

/// A workspace over the test task's directory with the `chat` actions registered and the panel
/// following `chats.enabled`, over the harness's runtime as the global one.
async fn panel_window<'a>(
    h: &Harness,
    cx: &'a mut TestAppContext,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        editor::init(cx);
        ChatRuntime::set_global(h.runtime.clone(), cx);
    });
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let project = Project::test(fs, [h.scope.as_path()], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(move |window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project, window, cx);
        multi_workspace.workspace().update(cx, |workspace, cx| {
            crate::register(workspace);
            init_panel(workspace, window, cx);
        });
        multi_workspace
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();
    (workspace, cx)
}

fn panel_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ChatPanel> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<ChatPanel>(cx).expect("the chats panel")
    })
}

async fn wait_indexed(h: &Harness, id: &str, cx: &mut TestAppContext) {
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(id).is_some())
    })
    .await;
}

fn show(panel: &Entity<ChatPanel>, id: &str, cx: &mut VisualTestContext) -> Entity<SessionView> {
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(id, window, cx)));
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| panel.session().expect("a session"))
}

async fn wait_prompts_loaded(session: &Entity<SessionView>, cx: &mut VisualTestContext) {
    wait_for(cx, "the prompts editor", STARTUP, |cx| {
        session.read_with(cx, |session, cx| session.prompts_editor(cx).is_some())
    })
    .await;
}

fn pane_count(session: &Entity<SessionView>, cx: &mut VisualTestContext) -> usize {
    session.read_with(cx, |session, _| session.visible_panes().len())
}

fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn toast_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<ChatToast>())
    })
}

fn layout_in_record(h: &Harness, id: &str) -> Layout {
    read_record(&record_path(&h.scope, id)).unwrap().layout
}

fn terminal_focused(session: &Entity<SessionView>, cx: &mut VisualTestContext) -> bool {
    let view = session
        .read_with(cx, |session, _| session.terminal_view().cloned())
        .expect("a terminal view");
    view.update_in(cx, |view, window, cx| {
        view.focus_handle(cx).is_focused(window)
    })
}

fn prompts_focused(session: &Entity<SessionView>, cx: &mut VisualTestContext) -> bool {
    let editor = session
        .read_with(cx, |session, cx| session.prompts_editor(cx))
        .expect("a prompts editor");
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).is_focused(window)
    })
}

#[gpui::test]
async fn test_dual_layout_shows_two_panes_and_chat_only_one(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let dual = h.start(Agent::Claude, cx).await;
    let mut request = h.request(Agent::Codex, "implement");
    request.record.layout = Layout::ChatOnly;
    write_record(&request.record_path, &request.record).unwrap();
    let chat_only = request.record.id.clone();
    wait_indexed(&h, &dual, cx).await;
    wait_indexed(&h, &chat_only, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);

    let session = show(&panel, &dual, cx);
    assert_eq!(pane_count(&session, cx), 2);
    wait_prompts_loaded(&session, cx).await;
    assert!(session.read_with(cx, |session, _| session.terminal_view().is_some()));

    let session = show(&panel, &chat_only, cx);
    assert_eq!(pane_count(&session, cx), 1);
    assert_eq!(
        session.read_with(cx, |session, cx| session.shown_layout(cx)),
        Layout::ChatOnly
    );
}

#[gpui::test]
async fn test_toggling_the_prompt_editor_keeps_the_buffer_and_the_terminal(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    wait_prompts_loaded(&session, cx).await;

    let editor = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("draft prompt", window, cx)
    });
    let dirty = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.read_with(cx, |editor, cx| editor.buffer().read(cx).is_dirty(cx))
    };
    assert!(dirty(&editor, cx));
    let text = editor.read_with(cx, |editor, cx| editor.text(cx));
    assert!(text.starts_with("draft prompt"));
    let terminal_view =
        session.read_with(cx, |session, _| session.terminal_view().cloned().unwrap());
    let terminal = terminal_view.read_with(cx, |view, _| view.terminal().clone());

    dispatch(cx, "chat::TogglePromptEditor");
    assert_eq!(pane_count(&session, cx), 1);
    wait_for(cx, "chat-only in the record", STARTUP, |_| {
        layout_in_record(&h, &id) == Layout::ChatOnly
    })
    .await;
    dispatch(cx, "chat::TogglePromptEditor");
    assert_eq!(pane_count(&session, cx), 2);
    wait_for(cx, "dual in the record", STARTUP, |_| {
        layout_in_record(&h, &id) == Layout::Dual
    })
    .await;

    let after = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());
    assert_eq!(after, editor, "the editor entity is the same");
    assert!(dirty(&after, cx), "the unsaved prompt is still unsaved");
    assert_eq!(after.read_with(cx, |editor, cx| editor.text(cx)), text);
    let terminal_view_after =
        session.read_with(cx, |session, _| session.terminal_view().cloned().unwrap());
    assert_eq!(terminal_view_after, terminal_view);
    assert_eq!(
        terminal_view_after.read_with(cx, |view, _| view.terminal().clone()),
        terminal
    );
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_the_layout_is_written_to_the_record_and_restored(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    assert_eq!(pane_count(&session, cx), 2);

    dispatch(cx, "chat::TogglePromptEditor");
    wait_for(cx, "chat-only in the record", STARTUP, |_| {
        layout_in_record(&h, &id) == Layout::ChatOnly
    })
    .await;
    // Wait for the store to index the write, then open the chat again in a new view.
    wait_for(cx, "the index to see the layout", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store
                .find(&id)
                .and_then(|chat| Some(chat.entry.record()?.layout))
                == Some(Layout::ChatOnly)
        })
    })
    .await;
    let restored = workspace.update_in(cx, |workspace, window, cx| {
        let (weak, project, fs) = (
            workspace.weak_handle(),
            workspace.project().clone(),
            workspace.app_state().fs.clone(),
        );
        cx.new(|cx| SessionView::new(id.clone(), weak, project, fs, window, cx))
    });
    assert_eq!(pane_count(&restored, cx), 1);

    // Toggling back writes dual.
    restored.update_in(cx, |session, window, cx| {
        session.toggle_prompt_editor(window, cx).unwrap()
    });
    assert_eq!(pane_count(&restored, cx), 2);
    wait_for(cx, "dual in the record", STARTUP, |_| {
        layout_in_record(&h, &id) == Layout::Dual
    })
    .await;
}

#[gpui::test]
async fn test_the_prompt_pane_setting_forces_chat_only(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    cx.update(|cx| {
        gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
            store.update_user_settings(cx, |content| {
                content.chats.get_or_insert_default().prompt_pane = Some(false);
            });
        });
    });
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    assert_eq!(pane_count(&session, cx), 1);

    // The toggle says why, and the record keeps its layout.
    dispatch(cx, "chat::TogglePromptEditor");
    assert!(toast_shown(&workspace, cx));
    assert_eq!(pane_count(&session, cx), 1);
    assert_eq!(layout_in_record(&h, &id), Layout::Dual);
    let refused = session.update_in(cx, |session, window, cx| {
        session.toggle_prompt_editor(window, cx)
    });
    assert_eq!(refused, Err(PROMPT_PANE_OFF));
}

#[gpui::test]
async fn test_the_panel_exists_only_while_chats_are_enabled(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let has_panel = |cx: &mut VisualTestContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<ChatPanel>(cx).is_some()
        })
    };
    assert!(!has_panel(cx));

    // Every handler says chats are off.
    for name in [
        "chat::TogglePanel",
        "chat::TogglePromptEditor",
        "chat::FocusTerminal",
        "chat::FocusPrompts",
    ] {
        workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
        dispatch(cx, name);
        assert!(toast_shown(&workspace, cx), "{name}");
    }

    set_chat_settings(cx, true, 60);
    cx.run_until_parked();
    assert!(has_panel(cx));
    set_chat_settings(cx, false, 60);
    cx.run_until_parked();
    assert!(!has_panel(cx));
}

#[gpui::test]
async fn test_the_chat_actions_dispatch_by_name(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);

    // No chat shown: the terminal and prompt commands say so.
    dispatch(cx, "chat::FocusTerminal");
    assert!(toast_shown(&workspace, cx));
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));

    let session = show(&panel, &id, cx);
    wait_prompts_loaded(&session, cx).await;

    // `chat: toggle panel` opens the dock and focuses the panel, never the terminal.
    dispatch(cx, "chat::TogglePanel");
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.right_dock().read(cx).is_open()
    }));
    assert!(!terminal_focused(&session, cx));
    assert!(!prompts_focused(&session, cx));

    dispatch(cx, "chat::FocusTerminal");
    assert!(terminal_focused(&session, cx));
    dispatch(cx, "chat::FocusPrompts");
    assert!(prompts_focused(&session, cx));
    assert!(!terminal_focused(&session, cx));

    // Focus prompts on a chat-only layout shows the pane again.
    dispatch(cx, "chat::TogglePromptEditor");
    assert_eq!(pane_count(&session, cx), 1);
    dispatch(cx, "chat::FocusPrompts");
    assert_eq!(pane_count(&session, cx), 2);
    assert!(prompts_focused(&session, cx));
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_focus_prompts_with_no_chat_needs_a_focused_task(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    dispatch(cx, "chat::FocusPrompts");
    assert!(toast_shown(&workspace, cx));
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).is_none()
    }));
}

fn prompts_shown(session: &Entity<SessionView>, cx: &mut VisualTestContext) -> Option<PathBuf> {
    session.read_with(cx, |session, _| {
        session.prompts_file().map(Path::to_path_buf)
    })
}

#[gpui::test]
async fn test_the_session_view_follows_the_prompts_file(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    wait_prompts_loaded(&session, cx).await;
    let own = h.scope.join("prompts.txt");
    assert_eq!(prompts_shown(&session, cx), Some(own.clone()));
    let before = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());

    let project_file = h.root.join("projects/invoicing/prompts.txt");
    std::fs::write(&project_file, "project prompt\n").unwrap();
    let set = cx.update(|_, cx| crate::set_prompts_file(&id, Some(project_file.clone()), cx));
    set.await.unwrap();
    wait_for(
        cx,
        "the session view on the project's file",
        STARTUP,
        |cx| {
            session.read_with(cx, |session, _| {
                session.prompts_file() == Some(&*project_file)
            })
        },
    )
    .await;
    let after = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());
    assert_ne!(after, before);
    assert_eq!(
        after.read_with(cx, |editor, cx| editor.text(cx)),
        "project prompt\n"
    );
    let items = session.read_with(cx, |session, cx| {
        session.prompts_pane().read(cx).items_len()
    });
    assert_eq!(items, 1, "the old editor is gone from the pane");

    // Back to the scope's own file.
    let set = cx.update(|_, cx| crate::set_prompts_file(&id, None, cx));
    set.await.unwrap();
    wait_for(cx, "the session view on the task's file", STARTUP, |cx| {
        session.read_with(cx, |session, _| session.prompts_file() == Some(&*own))
    })
    .await;
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_a_prompts_file_changed_away_and_back_ends_on_the_record(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    wait_prompts_loaded(&session, cx).await;
    let own = h.scope.join("prompts.txt");

    let project_file = h.root.join("projects/invoicing/prompts.txt");
    std::fs::write(&project_file, "project prompt\n").unwrap();
    // A to B to A in quick succession: B's load may still be running when the record is back on A.
    let set = cx.update(|_, cx| crate::set_prompts_file(&id, Some(project_file.clone()), cx));
    set.await.unwrap();
    let set = cx.update(|_, cx| crate::set_prompts_file(&id, None, cx));
    set.await.unwrap();
    wait_for(cx, "the index on the task's own file", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store.find(&id).is_some_and(|chat| {
                chat.entry
                    .record()
                    .is_some_and(|r| r.prompts_file.is_none())
            })
        })
    })
    .await;
    cx.executor()
        .timer(std::time::Duration::from_millis(300))
        .await;
    cx.run_until_parked();
    assert_eq!(prompts_shown(&session, cx), Some(own));
    let items = session.read_with(cx, |session, cx| {
        session.prompts_pane().read(cx).items_len()
    });
    assert_eq!(items, 1);
}

#[gpui::test]
async fn test_a_reload_never_drops_a_dirty_prompts_buffer(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &id, cx).await;
    let (workspace, cx) = panel_window(&h, cx).await;
    let panel = panel_of(&workspace, cx);
    let session = show(&panel, &id, cx);
    wait_prompts_loaded(&session, cx).await;
    let own = h.scope.join("prompts.txt");
    let editor = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());
    editor.update_in(cx, |editor, window, cx| {
        editor.insert("unsaved draft", window, cx)
    });
    let dirty = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.read_with(cx, |editor, cx| editor.buffer().read(cx).is_dirty(cx))
    };
    assert!(dirty(&editor, cx));

    let project_file = h.root.join("projects/invoicing/prompts.txt");
    let set = cx.update(|_, cx| crate::set_prompts_file(&id, Some(project_file.clone()), cx));
    set.await.unwrap();
    wait_for(cx, "the index to see the new prompts file", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store
                .find(&id)
                .and_then(|chat| chat.entry.record()?.prompts_file.clone())
                .is_some()
        })
    })
    .await;
    cx.executor()
        .timer(std::time::Duration::from_millis(300))
        .await;
    cx.run_until_parked();
    let shown = session.read_with(cx, |session, cx| session.prompts_editor(cx).unwrap());
    assert_eq!(shown, editor, "the dirty editor stays");
    assert!(dirty(&shown, cx));
    assert!(
        shown
            .read_with(cx, |editor, cx| editor.text(cx))
            .starts_with("unsaved draft")
    );
    assert_eq!(prompts_shown(&session, cx), Some(own));

    // Saved, the next change of the record opens the new file.
    let buffer = editor.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).as_singleton().unwrap()
    });
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    project
        .update(cx, |project, cx| project.save_buffer(buffer, cx))
        .await
        .unwrap();
    let set = cx.update(|_, cx| crate::set_auto_submit(&id, Some(false), cx));
    set.await.unwrap();
    wait_for(
        cx,
        "the session view on the project's file",
        STARTUP,
        |cx| {
            session.read_with(cx, |session, _| {
                session.prompts_file() == Some(&*project_file)
            })
        },
    )
    .await;
    assert!(
        std::fs::read_to_string(h.scope.join("prompts.txt"))
            .unwrap()
            .starts_with("unsaved draft")
    );
}
