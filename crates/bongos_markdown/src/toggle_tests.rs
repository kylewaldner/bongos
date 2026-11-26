//! Tests of `markdown: toggle live view` (per buffer, keeping point and the top line), the size
//! limit and its echo, the toggle outside markdown, and a click on a task's box.

use std::ops::Range;
use std::sync::Arc;

use bongos_minibuffer::minibuffer_for_window;
use editor::actions::Undo;
use editor::{Bias, Editor, MultiBufferOffset, SelectionEffects, ToOffset as _};
use gpui::{
    Action, AppContext as _, BorrowAppContext as _, Entity, Focusable as _, Modifiers,
    TestAppContext, VisualContext as _, VisualTestContext, WindowHandle, point,
};
use language::{Buffer, Language, LanguageConfig, LanguageRegistry};
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::conceals::CHECKBOX_SELECTOR;
use crate::{
    LARGE_FILE, Mode, NOT_MARKDOWN, conceal_tag, is_attached, markdown, mode_for_buffer, set_mode,
};

fn markdown_language() -> Arc<Language> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../grammars/src/markdown/config.toml"
    );
    let config = LanguageConfig::load(path).expect("the markdown config loads");
    Arc::new(Language::new(config, None))
}

struct Env {
    workspace: Entity<Workspace>,
    cx: VisualTestContext,
}

/// A workspace with no editor yet.
async fn setup(cx: &mut TestAppContext) -> Env {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        bongos_emacs::init(cx);
        bongos_minibuffer::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    Env { workspace, cx }
}

impl Env {
    fn buffer(&mut self, text: &str, language: Option<Arc<Language>>) -> Entity<Buffer> {
        let text = text.to_string();
        self.cx.new(|cx| {
            let mut buffer = Buffer::local(text, cx);
            buffer.set_language(language, cx);
            buffer
        })
    }

    /// A markdown buffer of `text` in `mode`.
    fn markdown(&mut self, text: &str, mode: Mode) -> Entity<Buffer> {
        let buffer = self.buffer(text, Some(markdown_language()));
        self.cx.update(|_, cx| set_mode(&buffer, mode, cx));
        buffer
    }

    /// A new editor on `buffer`, made the active item and focused.
    fn open(&mut self, buffer: &Entity<Buffer>) -> Entity<Editor> {
        let buffer = buffer.clone();
        let editor = self
            .cx
            .new_window_entity(|window, cx| Editor::for_buffer(buffer, None, window, cx));
        self.activate(&editor);
        editor
    }

    fn activate(&mut self, editor: &Entity<Editor>) {
        self.workspace
            .update_in(&mut self.cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
                window.focus(&editor.focus_handle(cx), cx);
            });
        self.cx.run_until_parked();
    }

    fn run(&mut self, action: impl Action) {
        self.cx.dispatch_action(action);
        self.cx.run_until_parked();
    }

    fn mode(&mut self, buffer: &Entity<Buffer>) -> Option<Mode> {
        self.cx.update(|_, cx| mode_for_buffer(buffer, cx))
    }

    fn text(&mut self, buffer: &Entity<Buffer>) -> String {
        buffer.read_with(&self.cx, |buffer, _| buffer.text())
    }

    /// Whether `editor` shows the live typography.
    fn styled(&mut self, editor: &Entity<Editor>) -> bool {
        editor.read_with(&self.cx, |editor, _| {
            editor.text_style_refinement().is_some()
        })
    }

    fn conceals(&mut self, editor: &Entity<Editor>) -> Vec<Range<usize>> {
        editor.update(&mut self.cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let buffer = display.buffer_snapshot();
            display
                .folds_in_range(MultiBufferOffset(0)..buffer.len())
                .filter(|fold| fold.placeholder.type_tag == Some(conceal_tag()))
                .map(|fold| {
                    fold.range.start.to_offset(buffer).0..fold.range.end.to_offset(buffer).0
                })
                .collect()
        })
    }

    fn put_cursor(&mut self, editor: &Entity<Editor>, offset: usize) {
        editor.update_in(&mut self.cx, |editor, window, cx| {
            let at = MultiBufferOffset(offset);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([at..at]);
            });
        });
        self.cx.run_until_parked();
    }

    fn cursor(&mut self, editor: &Entity<Editor>) -> usize {
        editor.update(&mut self.cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<MultiBufferOffset>(&snapshot)
                .head()
                .0
        })
    }

    /// The buffer row at the top of `editor`'s view, as drawn.
    fn top_row(&mut self, editor: &Entity<Editor>) -> u32 {
        editor.update(&mut self.cx, |editor, cx| {
            let top = editor.scroll_position(cx).y.max(0.0).floor() as u32;
            let display = editor.display_snapshot(cx);
            let shown = editor::DisplayPoint::new(editor::display_map::DisplayRow(top), 0);
            display.display_point_to_point(shown, Bias::Left).row
        })
    }

    /// The echo area's log, newest first.
    fn echoed(&mut self) -> Vec<String> {
        let strip = self
            .cx
            .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
            .expect("the window has a strip");
        strip.read_with(&self.cx, |strip, _| {
            strip
                .messages()
                .into_iter()
                .map(|line| line.text.to_string())
                .collect()
        })
    }
}

fn set_markdown_settings(cx: &mut gpui::App, live: bool, max_size: u64) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            let markdown = content.markdown.get_or_insert_default();
            markdown.live_view = Some(live);
            markdown.live_view_max_file_size = Some(max_size);
        });
    });
}

const DOC: &str = "# Title\n\nSome **bold** text.\n\n- [ ] task\n- plain\n";

#[gpui::test]
async fn toggle_flips_one_buffer_and_not_another(cx: &mut TestAppContext) {
    let mut env = setup(cx).await;
    let first = env.markdown(DOC, Mode::Source);
    let second = env.markdown(DOC, Mode::Source);
    let other = env.open(&second);
    let split = env.open(&first);
    let editor = env.open(&first);
    assert!(!env.styled(&editor) && !env.styled(&split) && !env.styled(&other));

    env.run(markdown::ToggleLiveView);
    assert_eq!(env.mode(&first), Some(Mode::Live));
    assert_eq!(env.mode(&second), Some(Mode::Source));
    assert!(env.styled(&editor));
    assert!(!env.conceals(&editor).is_empty());
    assert!(env.styled(&split), "every editor of the buffer follows");
    assert!(!env.conceals(&split).is_empty());
    assert!(!env.styled(&other), "the other buffer stays in source");
    assert!(env.conceals(&other).is_empty());

    env.run(markdown::ToggleLiveView);
    assert_eq!(env.mode(&first), Some(Mode::Source));
    assert_eq!(env.mode(&second), Some(Mode::Source));
    assert!(!env.styled(&editor) && !env.styled(&split));
    assert!(env.conceals(&editor).is_empty());

    // Toggling the other buffer leaves the first one alone.
    env.activate(&other);
    env.run(markdown::ToggleLiveView);
    assert_eq!(env.mode(&second), Some(Mode::Live));
    assert_eq!(env.mode(&first), Some(Mode::Source));
    assert!(env.styled(&other) && !env.styled(&editor));
    assert_eq!(env.text(&first), DOC);
    assert_eq!(env.text(&second), DOC);
}

#[gpui::test]
async fn toggle_keeps_point_and_the_top_line_both_ways(cx: &mut TestAppContext) {
    let mut env = setup(cx).await;
    let text: String = (0..300)
        .map(|i| match i % 4 {
            0 => format!("## Heading {i}\n"),
            1 => format!("Some **bold {i}** and [a link](http://x.y/{i}).\n"),
            2 => format!("- [ ] task {i}\n"),
            _ => "\n".to_string(),
        })
        .collect();
    let buffer = env.markdown(&text, Mode::Source);
    let editor = env.open(&buffer);
    editor.update_in(&mut env.cx, |editor, window, cx| {
        editor.set_scroll_position(point(0.0, 120.0), window, cx);
    });
    env.cx.run_until_parked();
    let top = env.top_row(&editor);
    assert_eq!(top, 120);
    // Point on a line in view, inside the bold text of row 125.
    let at = text.find("bold 125").expect("row 125") + 2;
    env.put_cursor(&editor, at);
    assert_eq!(env.top_row(&editor), top);

    for (round, mode) in [Mode::Live, Mode::Source, Mode::Live, Mode::Source]
        .into_iter()
        .enumerate()
    {
        env.run(markdown::ToggleLiveView);
        assert_eq!(env.mode(&buffer), Some(mode), "round {round}");
        assert_eq!(env.cursor(&editor), at, "round {round}: point stays");
        assert_eq!(
            env.top_row(&editor),
            top,
            "round {round}: the top line stays"
        );
    }
    assert_eq!(env.text(&buffer), text);
}

#[gpui::test]
async fn a_click_on_a_checkbox_flips_it_in_one_undo_step(cx: &mut TestAppContext) {
    let mut env = setup(cx).await;
    let text = "intro\n\n- [ ] task\n- plain\n";
    let buffer = env.markdown(text, Mode::Live);
    let editor = env.open(&buffer);
    env.put_cursor(&editor, 0);
    let task = text.find("- [ ]").expect("the task");
    assert!(
        env.conceals(&editor)
            .contains(&(task..task + "- [ ] ".len())),
        "the box is drawn as a glyph: {:?}",
        env.conceals(&editor)
    );

    let click = |env: &mut Env| {
        let bounds = env
            .cx
            .debug_bounds(CHECKBOX_SELECTOR)
            .expect("the box is drawn");
        env.cx.simulate_click(bounds.center(), Modifiers::none());
        env.cx.run_until_parked();
    };
    let checked = "intro\n\n- [x] task\n- plain\n";
    click(&mut env);
    assert_eq!(env.text(&buffer), checked);
    assert_eq!(env.cursor(&editor), 0, "the click does not move point");
    assert!(
        env.conceals(&editor)
            .contains(&(task..task + "- [x] ".len()))
    );
    click(&mut env);
    assert_eq!(env.text(&buffer), text);
    assert_eq!(env.cursor(&editor), 0);

    let undo = |env: &mut Env| {
        editor.update_in(&mut env.cx, |editor, window, cx| {
            editor.undo(&Undo, window, cx)
        });
        env.cx.run_until_parked();
    };
    undo(&mut env);
    assert_eq!(env.text(&buffer), checked, "each click is one undo step");
    undo(&mut env);
    assert_eq!(env.text(&buffer), text);
}

#[gpui::test]
async fn a_buffer_over_the_limit_opens_in_source_with_the_echo_and_toggles_on(
    cx: &mut TestAppContext,
) {
    let mut env = setup(cx).await;
    env.cx.update(|_, cx| set_markdown_settings(cx, true, 64));
    let small = env.buffer("# Small\n\n**b**\n", Some(markdown_language()));
    let small_editor = env.open(&small);
    assert_eq!(env.mode(&small), Some(Mode::Live));
    assert!(!env.echoed().iter().any(|line| line == LARGE_FILE));
    assert!(env.styled(&small_editor));

    let text = "# Large\n\nSome **bold** text that runs on.\n\n- [ ] a task over the limit\n";
    assert!(text.len() > 64);
    let large = env.buffer(text, Some(markdown_language()));
    let editor = env.open(&large);
    assert_eq!(env.mode(&large), Some(Mode::Source));
    assert_eq!(env.echoed().first().map(String::as_str), Some(LARGE_FILE));
    assert!(!env.styled(&editor));
    assert!(env.conceals(&editor).is_empty());

    env.run(markdown::ToggleLiveView);
    assert_eq!(env.mode(&large), Some(Mode::Live));
    assert!(env.styled(&editor));
    assert!(!env.conceals(&editor).is_empty());
    assert_eq!(env.text(&large), text);
    assert_eq!(env.mode(&small), Some(Mode::Live));

    // With the live view off in the settings, a large buffer opens in source without the echo.
    env.cx.update(|_, cx| set_markdown_settings(cx, false, 64));
    let before = env.echoed().len();
    let quiet = env.buffer(text, Some(markdown_language()));
    env.open(&quiet);
    assert_eq!(env.mode(&quiet), Some(Mode::Source));
    assert_eq!(env.echoed().len(), before);
}

#[gpui::test]
async fn the_toggle_in_a_prompts_file_echoes_and_changes_nothing(cx: &mut TestAppContext) {
    let mut env = setup(cx).await;
    let registry = Arc::new(LanguageRegistry::test(env.cx.executor()));
    bongos_prompts::register_language(&registry);
    let prompts_language = registry
        .language_for_name("Prompts")
        .await
        .expect("the prompts language");
    let text = "=== prompt ===\n# hello **there**\n";
    let prompts = env.buffer(text, Some(prompts_language));
    let editor = env.open(&prompts);
    assert!(editor.read_with(&env.cx, |editor, _| bongos_prompts::is_attached(editor)));

    env.run(markdown::ToggleLiveView);
    assert_eq!(env.echoed().first().map(String::as_str), Some(NOT_MARKDOWN));
    assert_eq!(env.mode(&prompts), None);
    assert!(!editor.read_with(&env.cx, |editor, _| is_attached(editor)));
    assert!(!env.styled(&editor));
    assert!(env.conceals(&editor).is_empty());
    assert_eq!(env.text(&prompts), text);

    // A buffer that is not markdown says the same.
    let plain = env.buffer("# not markdown\n", None);
    let plain_editor = env.open(&plain);
    let echoes = env.echoed().len();
    env.run(markdown::ToggleLiveView);
    assert_eq!(env.echoed().len(), echoes + 1);
    assert_eq!(env.echoed().first().map(String::as_str), Some(NOT_MARKDOWN));
    assert_eq!(env.mode(&plain), None);
    assert!(!env.styled(&plain_editor));
}
