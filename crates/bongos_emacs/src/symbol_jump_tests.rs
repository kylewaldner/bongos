use std::cell::{Cell, RefCell};
use std::rc::Rc;

use editor::test::editor_test_context::EditorTestContext;
use editor::{Editor, HighlightKey};
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, Hsla, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::{Fs as _, Project};
use settings::{SettingsStore, ThemeName, ThemeSelection};
use theme::{ActiveTheme as _, ThemeRegistry};
use workspace::{AppState, MinibufferRouting, MultiWorkspace, Workspace, WorkspaceHandle as _};

use crate::actions::{SymbolJumpNext, SymbolJumpPrev, UniversalArgument};
use crate::apply_tests::init_test;
use crate::prefix_arg;
use crate::symbol_jump::{FLASH_PEAK, FLASH_STEP, FLASH_STEPS};

/// The themes of the One family, as they ship with the app.
const ONE_THEMES: &str = include_str!("../../../assets/themes/one/one.json");

async fn context(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state(marked);
    cx
}

fn echo(cx: &mut EditorTestContext) -> Rc<RefCell<Vec<String>>> {
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

/// The flash ranges as (row, start column, end column) with their colors.
fn flash(cx: &mut EditorTestContext) -> Vec<((u32, u32, u32), Hsla)> {
    cx.update_editor(|editor, window, cx| {
        editor
            .all_text_background_highlights(window, cx)
            .into_iter()
            .map(|(range, color)| {
                (
                    (
                        range.start.row().0,
                        range.start.column(),
                        range.end.column(),
                    ),
                    color,
                )
            })
            .collect()
    })
}

fn has_flash(cx: &mut EditorTestContext) -> bool {
    cx.update_editor(|editor, _, _| editor.has_background_highlights(HighlightKey::SymbolJumpFlash))
}

fn alpha_of_active_match(cx: &mut EditorTestContext) -> f32 {
    cx.update(|_, cx| cx.theme().colors().search_active_match_background.a)
}

#[gpui::test]
async fn next_and_prev_land_per_the_model_and_keep_one_caret(cx: &mut TestAppContext) {
    let mut cx = context(cx, "fˇoo bar foo baz foo").await;
    cx.dispatch_action(SymbolJumpNext);
    cx.assert_editor_state("foo bar fˇoo baz foo");
    cx.dispatch_action(SymbolJumpNext);
    cx.assert_editor_state("foo bar foo baz fˇoo");
    cx.dispatch_action(SymbolJumpPrev);
    cx.assert_editor_state("foo bar fˇoo baz foo");

    cx.set_state("fˇoo bar fˇoo baz foo");
    cx.dispatch_action(SymbolJumpNext);
    let carets = cx.update_editor(|editor, _, _| editor.selections.count());
    assert_eq!(carets, 1);
}

#[gpui::test]
async fn wrapped_and_no_symbol_are_echoed(cx: &mut TestAppContext) {
    let mut cx = context(cx, "ˇfoo bar foo").await;
    let messages = echo(&mut cx);
    cx.dispatch_action(SymbolJumpNext);
    cx.assert_editor_state("foo bar ˇfoo");
    assert!(messages.borrow().is_empty());

    cx.dispatch_action(SymbolJumpNext);
    cx.assert_editor_state("ˇfoo bar foo");
    assert_eq!(*messages.borrow(), ["wrapped"]);

    cx.set_state("foo ˇ bar foo");
    cx.dispatch_action(SymbolJumpPrev);
    cx.assert_editor_state("foo ˇ bar foo");
    assert_eq!(*messages.borrow(), ["wrapped", "No symbol at point"]);
}

#[gpui::test]
async fn an_active_region_is_ignored(cx: &mut TestAppContext) {
    let mut cx = context(cx, "«foo bar fˇ»oo baz").await;
    cx.dispatch_action(SymbolJumpNext);
    cx.assert_editor_state("fˇoo bar foo baz");
}

#[gpui::test]
async fn the_flash_marks_the_landed_range_fades_and_goes(cx: &mut TestAppContext) {
    let mut cx = context(cx, "fˇoo bar foo baz foo").await;
    cx.dispatch_action(SymbolJumpNext);
    let alpha = alpha_of_active_match(&mut cx) * FLASH_PEAK;
    let shown = flash(&mut cx);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, (0, 8, 11));
    assert_eq!(shown[0].1.a, alpha);

    // Each step fades it a quarter more.
    for step in 1..FLASH_STEPS {
        cx.executor().advance_clock(FLASH_STEP);
        cx.run_until_parked();
        let shown = flash(&mut cx);
        assert_eq!(shown.len(), 1, "step {step}");
        let expected = alpha * (1.0 - step as f32 / FLASH_STEPS as f32);
        assert!((shown[0].1.a - expected).abs() < 1e-6, "step {step}");
    }
    cx.executor().advance_clock(FLASH_STEP);
    cx.run_until_parked();
    assert!(!has_flash(&mut cx));

    // A second jump moves it, and a third while it shows takes over from the older fade.
    cx.dispatch_action(SymbolJumpNext);
    assert_eq!(flash(&mut cx)[0].0, (0, 16, 19));
    cx.executor().advance_clock(FLASH_STEP * 3);
    cx.run_until_parked();
    cx.dispatch_action(SymbolJumpPrev);
    let shown = flash(&mut cx);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, (0, 8, 11));
    assert_eq!(shown[0].1.a, alpha);
    cx.executor().advance_clock(FLASH_STEP);
    cx.run_until_parked();
    let shown = flash(&mut cx);
    assert_eq!(shown.len(), 1);
    assert!((shown[0].1.a - alpha * 0.75).abs() < 1e-6);
    cx.executor().advance_clock(FLASH_STEP * 3);
    cx.run_until_parked();
    assert!(!has_flash(&mut cx));
}

async fn file_editor(
    text: &str,
    cx: &mut TestAppContext,
) -> (Entity<Editor>, VisualTestContext, Entity<Workspace>) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    let fake = app_state.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    fake.insert_file("/root/one.txt", text.as_bytes().to_vec())
        .await;
    let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let path = cx.read(|cx| workspace.file_project_paths(cx)).remove(0);
    let editor = workspace
        .update_in(&mut cx, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    cx.run_until_parked();
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    (editor, cx, workspace)
}

#[gpui::test]
async fn a_jump_changes_no_text_undo_entry_or_navigation_entry(cx: &mut TestAppContext) {
    let text = format!("word\n{}word\n", "filler\n".repeat(30));
    let (editor, mut cx, workspace) = file_editor(&text, cx).await;
    let history = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton().unwrap();
            buffer.read_with(cx, |buffer, _| {
                (
                    buffer.text(),
                    buffer.version(),
                    buffer.peek_undo_stack().map(|entry| entry.transaction_id()),
                )
            })
        })
    };
    let has_back_entry = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
        workspace.update(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .nav_history()
                .last_backward_where(&mut |_, _| true, cx)
        })
    };
    let before = history(&editor, &mut cx);
    assert!(!has_back_entry(&workspace, &mut cx));

    cx.dispatch_action(SymbolJumpNext);
    cx.run_until_parked();
    let row = editor.update(&mut cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.newest_display(&display).head().row().0
    });
    assert_eq!(row, 31, "the caret moved");
    assert!(history(&editor, &mut cx) == before);
    assert!(!has_back_entry(&workspace, &mut cx));
}

fn use_theme(name: &str, cx: &mut EditorTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.theme.theme = Some(ThemeSelection::Static(ThemeName(name.into())));
            });
        });
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.theme().name.as_ref(), name, "the theme is active"));
}

/// WCAG contrast ratio of two opaque colors.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    fn luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        let linear = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[gpui::test]
async fn the_flash_is_readable_and_follows_the_theme_in_one_light_and_one_dark(
    cx: &mut TestAppContext,
) {
    let mut cx = context(cx, "fˇoo bar foo").await;
    cx.update(|_, cx| {
        let registry = ThemeRegistry::global(cx);
        theme_settings::load_user_theme(&registry, ONE_THEMES.as_bytes())
            .expect("the One themes load");
    });
    cx.dispatch_action(SymbolJumpNext);

    let mut seen = Vec::new();
    for name in ["One Light", "One Dark"] {
        // The flash was drawn once; the color is read from the theme each time.
        use_theme(name, &mut cx);
        let shown = flash(&mut cx);
        assert_eq!(shown.len(), 1, "{name}");
        let (background, text) = cx.update(|_, cx| {
            let colors = cx.theme().colors();
            (colors.editor_background, colors.editor_foreground)
        });
        let blended = background.blend(shown[0].1);
        assert_ne!(blended, background, "{name}: the flash shows");
        let ratio = contrast(blended, text);
        assert!(ratio >= 4.5, "{name}: contrast {ratio}");
        seen.push(shown[0].1);
    }
    assert_ne!(seen[0], seen[1], "the color follows the theme");
}

#[gpui::test]
async fn a_single_line_editor_propagates_the_jumps(cx: &mut TestAppContext) {
    init_test(cx);
    let passed = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let count = passed.clone();
        cx.on_action(move |_: &SymbolJumpNext, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &SymbolJumpPrev, _| count.set(count.get() + 1));
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two one", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));

    cx.dispatch_action(SymbolJumpNext);
    cx.dispatch_action(SymbolJumpPrev);
    assert_eq!(passed.get(), 2);
    assert!(!editor.update(cx, |editor, _| {
        editor.has_background_highlights(HighlightKey::SymbolJumpFlash)
    }));
}

/// What the key interceptor does when a bound key follows C-u: the argument goes to the command.
fn hand_off(cx: &mut EditorTestContext) {
    cx.update(|window, cx| prefix_arg::hand_off(window, cx));
}

#[gpui::test]
async fn the_commands_ignore_the_argument(cx: &mut TestAppContext) {
    let marked = "fˇoo bar foo baz foo";
    let mut plain = context(cx, marked).await;
    plain.dispatch_action(SymbolJumpNext);
    let plain_state = plain.editor_state();
    drop(plain);

    let mut counted = context(cx, marked).await;
    counted.dispatch_action(UniversalArgument);
    hand_off(&mut counted);
    counted.dispatch_action(SymbolJumpNext);
    assert_eq!(counted.editor_state(), plain_state);
    let pending = counted.update(|window, cx| prefix_arg::pending(window, cx));
    assert!(pending.is_none());

    counted.set_state(marked);
    counted.dispatch_action(UniversalArgument);
    hand_off(&mut counted);
    counted.dispatch_action(SymbolJumpPrev);
    counted.assert_editor_state("foo bar foo baz fˇoo");
    let pending = counted.update(|window, cx| prefix_arg::pending(window, cx));
    assert!(pending.is_none());
}
