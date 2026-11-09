//! Tests of the settings, the `Prompts` language and the `prompts` actions.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use command_palette_hooks::CommandPaletteFilter;
use fs::{FakeFs, Fs as _};
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use language::language_settings::{AllLanguageSettings, FormatOnSave};
use language::{LanguageName, LanguageRegistry, PLAIN_TEXT};
use project::Project;
use project::lsp_store::{FormatTrigger, LspFormatTarget};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{PromptsSettings, PromptsToast, action_types, register_language};

/// The names of every `prompts` action with the command the palette shows for it.
const COMMANDS: &[(&str, &str)] = &[
    ("prompts::Send", "prompts: send"),
    (
        "prompts::SendWithoutMarking",
        "prompts: send without marking",
    ),
    ("prompts::SendRegion", "prompts: send region"),
    ("prompts::New", "prompts: new"),
    ("prompts::Previous", "prompts: previous"),
    ("prompts::Next", "prompts: next"),
    ("prompts::NextPending", "prompts: next pending"),
    ("prompts::Copy", "prompts: copy"),
    ("prompts::UseFile", "prompts: use file"),
    ("prompts::UseDefault", "prompts: use default"),
    ("prompts::ToggleAutoSubmit", "prompts: toggle auto-submit"),
    ("prompts::Open", "prompts: open"),
    ("prompts::Mode", "prompts: mode"),
];

/// The settings the app starts with: the real `default.json`, languages included (the shared test
/// settings drop the `languages` block).
fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn prompts_txt_gets_the_prompts_language_and_notes_txt_plain_text(cx: &mut TestAppContext) {
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    register_language(&registry);
    for (path, expected) in [
        ("prompts.txt", "Prompts"),
        ("/home/me/.bongos/work/projects/a/prompts.txt", "Prompts"),
        ("foo.prompts.txt", "Prompts"),
        ("notes.txt", "Plain Text"),
        ("myprompts.txt", "Plain Text"),
    ] {
        let name = registry
            .language_for_file_path(Path::new(path))
            .map(|available| available.name().to_string());
        assert_eq!(name.as_deref(), Some(expected), "{path}");
    }
    let language = registry.language_for_name("Prompts").await.unwrap();
    assert_eq!(language.name().as_ref(), "Prompts");
    assert!(language.grammar().is_none(), "Prompts has no grammar");
    // The built-in Plain Text is still the one for `.txt`.
    assert_eq!(PLAIN_TEXT.name().as_ref(), "Plain Text");
}

#[gpui::test]
async fn registering_the_language_twice_keeps_one(cx: &mut TestAppContext) {
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    register_language(&registry);
    register_language(&registry);
    let count = registry
        .language_names()
        .into_iter()
        .filter(|name| name.as_ref() == "Prompts")
        .count();
    assert_eq!(count, 1);
}

#[gpui::test]
async fn default_settings_parse(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        assert!(PromptsSettings::get_global(cx).enabled, "on by default");

        let language = LanguageName::new("Prompts");
        let prompts = AllLanguageSettings::get_global(cx).language(None, Some(&language), cx);
        assert!(!prompts.remove_trailing_whitespace_on_save);
        assert!(!prompts.ensure_final_newline_on_save);
        assert!(matches!(prompts.format_on_save, FormatOnSave::Off));
        assert!(!prompts.use_autoclose);
        assert!(!prompts.use_auto_surround);
        assert!(!prompts.auto_indent_on_paste);
        assert!(!prompts.show_completions_on_input);
        assert!(!prompts.show_edit_predictions);
        assert!(!prompts.enable_language_server);
        assert!(matches!(
            prompts.auto_indent,
            settings::AutoIndentMode::None
        ));
        assert!(matches!(prompts.soft_wrap, settings::SoftWrap::EditorWidth));
        assert!(PromptsSettings::get_global(cx).auto_submit);
        assert!(PromptsSettings::get_global(cx).save_on_send);

        // The global defaults, and Plain Text, still rewrite.
        let plain = AllLanguageSettings::get_global(cx).language(
            None,
            Some(&LanguageName::new("Plain Text")),
            cx,
        );
        assert!(plain.remove_trailing_whitespace_on_save);
        assert!(plain.ensure_final_newline_on_save);
    });
}

/// Saves `text` to `name` through the format-on-save path, and returns the bytes on disk and
/// the language the buffer got.
async fn save_exactly(cx: &mut TestAppContext, name: &str, text: &str) -> (String, String) {
    use_default_settings(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", serde_json::json!({ (name): text }))
        .await;
    let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
    let registry = project.read_with(cx, |project, _| project.languages().clone());
    register_language(&registry);
    let path = format!("/dir/{name}");
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(&path, cx))
        .await
        .unwrap();
    cx.run_until_parked();
    let language = buffer.read_with(cx, |buffer, _| {
        buffer
            .language()
            .map_or_else(String::new, |language| language.name().to_string())
    });
    project
        .update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                false,
                FormatTrigger::Save,
                cx,
            )
        })
        .await
        .unwrap();
    project
        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .unwrap();
    (fs.load(Path::new(&path)).await.unwrap(), language)
}

#[gpui::test]
async fn saving_a_prompts_buffer_writes_the_same_bytes(cx: &mut TestAppContext) {
    let text =
        "--- prompt 1 ---\nsay hi   \n\tindented with a tab\t\n\n   \nlast line, no newline  ";
    let (saved, language) = save_exactly(cx, "prompts.txt", text).await;
    assert_eq!(language, "Prompts");
    assert_eq!(saved, text);
}

#[gpui::test]
async fn saving_plain_text_still_tidies_it(cx: &mut TestAppContext) {
    // The control of the test above: the same text in `notes.txt` is rewritten.
    let text = "say hi   \nlast line, no newline  ";
    let (saved, language) = save_exactly(cx, "notes.txt", text).await;
    assert_eq!(language, "Plain Text");
    assert_eq!(saved, "say hi\nlast line, no newline\n");
}

fn palette_hidden(name: &str, cx: &mut TestAppContext) -> bool {
    cx.update(|cx| {
        let action = cx
            .build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"));
        CommandPaletteFilter::try_global(cx)
            .expect("the palette filter exists")
            .is_hidden(&*action)
    })
}

#[gpui::test]
async fn actions_are_hidden_while_off_and_shown_while_on(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    set_enabled(cx, false);
    cx.update(crate::init_palette);

    let mut names: Vec<&str> = COMMANDS.iter().map(|(name, _)| *name).collect();
    #[cfg(debug_assertions)]
    names.push("prompts::DumpView");
    assert_eq!(names.len(), action_types().len(), "every action is listed");
    for name in &names {
        assert!(palette_hidden(name, cx), "{name} is hidden while off");
    }

    set_enabled(cx, true);
    for name in &names {
        assert!(!palette_hidden(name, cx), "{name} shows while on");
    }

    set_enabled(cx, false);
    for name in &names {
        assert!(palette_hidden(name, cx), "{name} is hidden again");
    }
}

async fn workspace_with_actions(
    cx: &mut TestAppContext,
) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update_in(cx, |workspace, window, cx| {
        register_for_test(workspace);
        window.focus(&gpui::Focusable::focus_handle(workspace, cx), cx)
    });
    cx.run_until_parked();
    (workspace, cx)
}

fn register_for_test(workspace: &mut Workspace) {
    crate::register(workspace);
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn toast_shown(workspace: &gpui::Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<PromptsToast>())
    })
}

#[gpui::test]
async fn dispatch_by_name_shows_the_off_toast(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    let mut commands: Vec<(&str, &str)> = COMMANDS.to_vec();
    #[cfg(debug_assertions)]
    commands.push(("prompts::DumpView", "prompts: dump view"));
    for (name, command) in commands {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<PromptsToast>(), cx)
        });
        assert!(!toast_shown(&workspace, cx));
        assert_eq!(
            cx.update(|_, cx| cx.build_action(name, None).unwrap().name()),
            name
        );
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
        assert!(!command.is_empty());
    }
}

#[test]
fn the_off_toast_names_the_setting() {
    assert_eq!(
        crate::actions::OFF_MESSAGE,
        "Prompts mode is off (prompts.enabled)"
    );
}

#[gpui::test]
async fn no_toast_while_on(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    dispatch("prompts::Open", cx);
    assert!(!toast_shown(&workspace, cx));
}
