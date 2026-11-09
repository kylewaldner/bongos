//! Tests of the prompts mode in a real window: attach and detach, the rules in place of marker
//! lines, the current and sent highlights, the mode line in the center and in the Chats panel's
//! session header, 5,000 blocks, and the index fed one merged range per row.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use bongos_chat::PromptPaneProvider as _;
use editor::{Editor, HighlightKey, SelectionEffects, ToPoint as _};
use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, Focusable as _, TestAppContext,
    VisualTestContext,
};
use language::{Buffer, Point};
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, OpenOptions, StatusItemView as _, Workspace};

use crate::{
    PromptsCurrent, PromptsModeLine, PromptsPaneProvider, dump_text, is_attached, mode_line,
    mode_of, register_language,
};

const THREE: &str = "\
preamble
=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===
first, sent
=== prompt 2026-11-05 14:10 ===
second
  more
=== prompt 2026-11-05 14:30 ===
third";

pub(crate) fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

/// Settings, theme, editor and the prompts mode; returns the app state with its fake file system.
pub(crate) fn init(cx: &mut TestAppContext, enabled: bool) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    set_enabled(cx, enabled);
    app_state
}

pub(crate) async fn open_buffer(
    cx: &mut TestAppContext,
    enabled: bool,
    name: &str,
    text: &str,
) -> (Entity<Project>, Entity<Buffer>) {
    let app_state = init(cx, enabled);
    app_state
        .fs
        .as_fake()
        .insert_tree("/dir", serde_json::json!({ (name): text }))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    register_language(&project.read_with(cx, |project, _| project.languages().clone()));
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(format!("/dir/{name}"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    (project, buffer)
}

pub(crate) async fn open_editor<'a>(
    cx: &'a mut TestAppContext,
    enabled: bool,
    name: &str,
    text: &str,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    let (project, buffer) = open_buffer(cx, enabled, name, text).await;
    let (editor, cx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(buffer, Some(project), window, cx));
    cx.run_until_parked();
    (editor, cx)
}

pub(crate) fn attached(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, _| is_attached(editor))
}

fn rule_count(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
    editor.read_with(cx, |editor, _| {
        mode_of(editor).map_or(0, |mode| mode.borrow().rule_count())
    })
}

fn lines_parsed(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> u64 {
    editor.read_with(cx, |editor, _| {
        mode_of(editor)
            .expect("the mode")
            .borrow()
            .index()
            .stats()
            .lines_parsed
    })
}

pub(crate) fn display_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    editor.update(cx, |editor, cx| editor.display_text(cx))
}

pub(crate) fn move_to(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([Point::new(row, 0)..Point::new(row, 0)])
        })
    });
    cx.run_until_parked();
}

pub(crate) fn edit(
    editor: &Entity<Editor>,
    range: Range<Point>,
    text: &str,
    cx: &mut VisualTestContext,
) {
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(range, text)], None, cx));
    });
    cx.run_until_parked();
}

/// The rows (first to last) of each range of the current-block row highlight.
fn current_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<u32>> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .highlighted_rows::<PromptsCurrent>(cx)
            .map(|(range, _)| {
                range.start.to_point(&snapshot).row..range.end.to_point(&snapshot).row
            })
            .collect()
    })
}

/// The rows (first to last) of each dimmed sent body.
fn sent_rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<Range<u32>> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .text_highlights(HighlightKey::PromptsSent, cx)
            .map(|(_, ranges)| {
                ranges
                    .iter()
                    .map(|range| {
                        range.start.to_point(&snapshot).row..range.end.to_point(&snapshot).row
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

#[gpui::test]
async fn attaches_on_prompts_txt(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    assert!(attached(&editor, cx));
    assert_eq!(rule_count(&editor, cx), 3);
    // The key context of the mode.
    let context = editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx);
        editor.key_context(window, cx)
    });
    assert!(context.contains("prompts"), "{context:?}");
}

#[gpui::test]
async fn does_not_attach_on_notes_txt(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "notes.txt", THREE).await;
    assert!(!attached(&editor, cx));
    assert!(display_text(&editor, cx).contains("=== prompt 2026-11-05 14:10 ==="));
}

#[gpui::test]
async fn nothing_attaches_while_disabled_and_the_setting_attaches_and_detaches(
    cx: &mut TestAppContext,
) {
    let (editor, cx) = open_editor(cx, false, "prompts.txt", THREE).await;
    assert!(!attached(&editor, cx));
    let plain = display_text(&editor, cx);
    assert!(plain.contains("=== prompt 2026-11-05 14:10 ==="));
    assert!(current_rows(&editor, cx).is_empty());
    assert!(sent_rows(&editor, cx).is_empty());

    set_enabled(cx, true);
    cx.run_until_parked();
    assert!(attached(&editor, cx));
    assert!(!display_text(&editor, cx).contains("=== prompt"));

    set_enabled(cx, false);
    cx.run_until_parked();
    assert!(!attached(&editor, cx));
    assert_eq!(display_text(&editor, cx), plain, "rules removed");
    assert!(current_rows(&editor, cx).is_empty(), "highlights removed");
    assert!(sent_rows(&editor, cx).is_empty());
}

#[gpui::test]
async fn a_language_change_attaches_and_detaches(cx: &mut TestAppContext) {
    let (project, buffer) = open_buffer(cx, true, "notes.txt", THREE).await;
    let (editor, cx) = cx.add_window_view({
        let project = project.clone();
        let buffer = buffer.clone();
        |window, cx| Editor::for_buffer(buffer, Some(project), window, cx)
    });
    cx.run_until_parked();
    assert!(!attached(&editor, cx));

    let languages = project.read_with(cx, |project, _| project.languages().clone());
    let prompts = languages.language_for_name("Prompts").await.unwrap();
    project.update(cx, |project, cx| {
        project.set_language_for_buffer(&buffer, prompts, cx)
    });
    cx.run_until_parked();
    assert!(attached(&editor, cx), "prompts: mode's call attaches");

    let plain = languages.language_for_name("Plain Text").await.unwrap();
    project.update(cx, |project, cx| {
        project.set_language_for_buffer(&buffer, plain, cx)
    });
    cx.run_until_parked();
    assert!(!attached(&editor, cx), "another language detaches");
    assert!(display_text(&editor, cx).contains("=== prompt"));
}

#[gpui::test]
async fn display_text_has_a_rule_per_marker_and_no_marker_text(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    let display = display_text(&editor, cx);
    assert!(!display.contains("=== prompt"), "{display}");
    for body in ["preamble", "first, sent", "second", "  more", "third"] {
        assert!(display.contains(body), "{body} in {display}");
    }
    // A replace block of height 1 keeps the row: the display has as many rows as the buffer.
    assert_eq!(display.lines().count(), THREE.lines().count(), "{display}");
    let stats = editor.read_with(cx, |editor, _| mode_of(editor).unwrap().borrow().stats());
    assert_eq!(stats.insert_blocks_calls, 1);
}

#[gpui::test]
async fn rules_follow_typing_and_deleting_marker_lines(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    // A new marker line typed after "third".
    edit(
        &editor,
        Point::new(7, 5)..Point::new(7, 5),
        "\n=== prompt 2026-11-06 09:00 ===\nfourth",
        cx,
    );
    assert_eq!(rule_count(&editor, cx), 4);
    assert!(!display_text(&editor, cx).contains("=== prompt"));

    // The second marker line deleted, with its line break.
    edit(&editor, Point::new(3, 0)..Point::new(4, 0), "", cx);
    assert_eq!(rule_count(&editor, cx), 3);
    let display = display_text(&editor, cx);
    assert!(!display.contains("=== prompt"), "{display}");
    assert!(display.contains("second"));

    // A marker broken by typing in it is body text again.
    edit(&editor, Point::new(1, 0)..Point::new(1, 0), "x", cx);
    assert_eq!(rule_count(&editor, cx), 2);
    assert!(display_text(&editor, cx).contains("x=== prompt 2026-11-05 13:40"));

    let mode = editor.read_with(cx, |editor, _| mode_of(editor).unwrap());
    let markers: Vec<u32> = mode
        .borrow()
        .index()
        .markers()
        .iter()
        .map(|m| m.row)
        .collect();
    assert_eq!(markers, vec![5, 7]);
}

#[gpui::test]
async fn current_block_is_highlighted_and_sent_bodies_dimmed(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    // Point starts on row 0, the preamble.
    assert_eq!(current_rows(&editor, cx), vec![0..0]);
    assert_eq!(sent_rows(&editor, cx), vec![2..2]);

    move_to(&editor, 5, cx);
    assert_eq!(
        current_rows(&editor, cx),
        vec![4..5],
        "the second block's body"
    );
    assert_eq!(sent_rows(&editor, cx), vec![2..2]);

    // In the sent block, its body reads at full strength.
    move_to(&editor, 2, cx);
    assert_eq!(current_rows(&editor, cx), vec![2..2]);
    assert!(sent_rows(&editor, cx).is_empty());

    // A move inside the block keeps the same highlight.
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([Point::new(2, 3)..Point::new(2, 3)])
        })
    });
    cx.run_until_parked();
    assert_eq!(current_rows(&editor, cx), vec![2..2]);

    // Typing a newline grows the current block's highlight.
    edit(
        &editor,
        Point::new(5, 6)..Point::new(5, 6),
        "\nyet more",
        cx,
    );
    move_to(&editor, 6, cx);
    assert_eq!(current_rows(&editor, cx), vec![4..6]);
}

#[gpui::test]
async fn five_thousand_blocks_attach_in_one_insert_blocks(cx: &mut TestAppContext) {
    let mut text = String::new();
    for n in 0..5_000 {
        let sent = if n % 2 == 0 {
            " | sent 2026-11-05 14:02"
        } else {
            ""
        };
        text.push_str(&format!(
            "=== prompt 2026-11-05 13:40{sent} ===\nprompt number {n}\n"
        ));
    }
    let (editor, cx) = open_editor(cx, true, "prompts.txt", &text).await;
    let mode = editor.read_with(cx, |editor, _| mode_of(editor).unwrap());
    {
        let mode = mode.borrow();
        assert_eq!(mode.rule_count(), 5_000);
        assert_eq!(mode.stats().insert_blocks_calls, 1);
        assert_eq!(
            mode.index().block_count(),
            5_000,
            "the empty last row is a body"
        );
    }
    let parsed = lines_parsed(&editor, cx);
    {
        let mode = mode.borrow();
        let index = mode.index();
        assert_eq!(index.position(5_001).0, 2_501);
        assert!(index.next(4_000, 3).is_some());
        assert!(index.previous(4_000, 3).is_some());
        assert!(index.next_pending(4_000).is_some());
    }
    move_to(&editor, 9_001, cx);
    assert_eq!(
        lines_parsed(&editor, cx),
        parsed,
        "navigation parses zero lines"
    );
    let line = mode_line_of(&editor, cx).unwrap();
    assert!(line.starts_with("prompt 4501/5000 · "), "{line}");
}

fn mode_line_of(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<String> {
    editor.read_with(cx, |editor, cx| {
        mode_line(editor, cx).map(|line| line.to_string())
    })
}

#[gpui::test]
async fn two_cursors_on_one_row_parse_the_row_once(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    let before = lines_parsed(&editor, cx);
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges([
                Point::new(4, 1)..Point::new(4, 1),
                Point::new(4, 4)..Point::new(4, 4),
            ])
        });
        editor.handle_input("x", window, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        editor.read_with(cx, |editor, cx| editor
            .text(cx)
            .lines()
            .nth(4)
            .unwrap()
            .to_string()),
        "sxecoxnd"
    );
    assert_eq!(lines_parsed(&editor, cx) - before, 1, "one row touched");
}

#[gpui::test]
async fn the_mode_line_reads_the_block_at_point(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    assert_eq!(
        mode_line_of(&editor, cx).as_deref(),
        Some("prompt 1/4 · pending · auto-submit on")
    );
    move_to(&editor, 2, cx);
    assert_eq!(
        mode_line_of(&editor, cx).as_deref(),
        Some("prompt 2/4 · sent 14:02 · auto-submit on")
    );
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().auto_submit = Some(false);
            });
        });
    });
    move_to(&editor, 7, cx);
    assert_eq!(
        mode_line_of(&editor, cx).as_deref(),
        Some("prompt 4/4 · pending · auto-submit off")
    );

    let dump = cx.update(|_, cx| dump_text(&editor, cx)).unwrap();
    assert!(dump.contains("mode line: prompt 4/4 · pending"), "{dump}");
    assert!(dump.contains("rules: 3"), "{dump}");
    assert!(
        dump.contains("marker row 1: 2026-11-05 13:40 · sent 2026-11-05 14:02"),
        "{dump}"
    );
    assert!(dump.contains("display:\npreamble\n"), "{dump}");
    assert!(!dump.contains("=== prompt"), "{dump}");
}

#[gpui::test]
async fn the_status_item_shows_the_mode_line_of_the_active_editor(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "prompts.txt", THREE).await;
    let status = cx.update(|_, cx| cx.new(|_| PromptsModeLine::new()));
    assert_eq!(status.read_with(cx, |status, cx| status.text(cx)), None);
    status.update_in(cx, |status, window, cx| {
        status.set_active_pane_item(Some(&editor), window, cx)
    });
    assert_eq!(
        status
            .read_with(cx, |status, cx| status.text(cx))
            .as_deref(),
        Some("prompt 1/4 · pending · auto-submit on")
    );
    move_to(&editor, 4, cx);
    assert_eq!(
        status
            .read_with(cx, |status, cx| status.text(cx))
            .as_deref(),
        Some("prompt 3/4 · pending · auto-submit on")
    );
    set_enabled(cx, false);
    cx.run_until_parked();
    assert_eq!(status.read_with(cx, |status, cx| status.text(cx)), None);
}

#[gpui::test]
async fn the_status_item_is_empty_for_a_plain_editor(cx: &mut TestAppContext) {
    let (editor, cx) = open_editor(cx, true, "notes.txt", THREE).await;
    let status = cx.update(|_, cx| cx.new(|_| PromptsModeLine::new()));
    status.update_in(cx, |status, window, cx| {
        status.set_active_pane_item(Some(&editor), window, cx)
    });
    assert_eq!(status.read_with(cx, |status, cx| status.text(cx)), None);
}

#[gpui::test]
async fn the_pane_provider_builds_a_plain_editor_while_disabled(cx: &mut TestAppContext) {
    let (project, buffer) = open_buffer(cx, false, "prompts.txt", THREE).await;
    let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
    let item = cx.update(|window, cx| {
        PromptsPaneProvider.build_item(buffer.clone(), project.clone(), window, cx)
    });
    cx.run_until_parked();
    let editor = cx
        .update(|_, cx| item.act_as::<Editor>(cx))
        .expect("an editor");
    assert!(!attached(&editor, cx), "no addon while off");
    assert!(display_text(&editor, cx).contains("=== prompt"));
    assert_eq!(
        cx.update(|_, cx| PromptsPaneProvider.mode_line(item.as_ref(), cx)),
        None,
        "no mode line in the header while off"
    );
}

#[gpui::test]
async fn the_pane_provider_gives_the_session_header_its_mode_line(cx: &mut TestAppContext) {
    let (project, buffer) = open_buffer(cx, true, "prompts.txt", THREE).await;
    let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
    let item = cx.update(|window, cx| {
        PromptsPaneProvider.build_item(buffer.clone(), project.clone(), window, cx)
    });
    cx.run_until_parked();
    let editor = cx
        .update(|_, cx| item.act_as::<Editor>(cx))
        .expect("an editor");
    assert!(attached(&editor, cx));
    move_to(&editor, 2, cx);
    assert_eq!(
        cx.update(|_, cx| PromptsPaneProvider.mode_line(item.as_ref(), cx))
            .as_deref(),
        Some("prompt 2/4 · sent 14:02 · auto-submit on")
    );
}

pub(crate) async fn workspace_on(
    cx: &mut TestAppContext,
    enabled: bool,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let app_state = init(cx, enabled);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            "/dir",
            serde_json::json!({ "notes.txt": THREE, "prompts.txt": THREE }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    register_language(&project.read_with(cx, |project, _| project.languages().clone()));
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update(cx, |workspace, _| crate::register(workspace));
    (workspace, cx)
}

pub(crate) async fn open_in(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                PathBuf::from(format!("/dir/{name}")),
                OpenOptions::default(),
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
    cx.update(|_, cx| item.act_as::<Editor>(cx)).unwrap()
}

pub(crate) fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

#[gpui::test]
async fn prompts_mode_puts_the_active_file_in_the_mode(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_on(cx, true).await;
    let editor = open_in(&workspace, "notes.txt", cx).await;
    assert!(!attached(&editor, cx));
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    dispatch("prompts::Mode", cx);
    assert!(attached(&editor, cx));
    assert_eq!(rule_count(&editor, cx), 3);
}

#[gpui::test]
async fn prompts_mode_while_disabled_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_on(cx, false).await;
    let editor = open_in(&workspace, "notes.txt", cx).await;
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx)
    });
    dispatch("prompts::Mode", cx);
    assert!(!attached(&editor, cx));
    let language = editor.read_with(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer
            .read(cx)
            .language()
            .map(|language| language.name().to_string())
    });
    assert_eq!(language.as_deref(), Some("Plain Text"));
    assert!(workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&workspace::notifications::NotificationId::unique::<
                crate::PromptsToast,
            >())
    }));
}

#[gpui::test]
async fn opening_a_prompts_file_in_the_center_attaches(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_on(cx, true).await;
    let editor = open_in(&workspace, "prompts.txt", cx).await;
    assert!(attached(&editor, cx));
    let display = display_text(&editor, cx);
    assert!(!display.contains("=== prompt"), "{display}");
}
