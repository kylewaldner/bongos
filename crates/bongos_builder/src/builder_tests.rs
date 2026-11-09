//! Tests of the settings, the `Proposal` language, the actions and the format file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use command_palette_hooks::CommandPaletteFilter;
use fs::{FakeFs, Fs as _};
use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
use language::language_settings::{AllLanguageSettings, FormatOnSave};
use language::{LanguageName, LanguageRegistry};
use project::Project;
use project::lsp_store::{FormatTrigger, LspFormatTarget};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::format_file::write_if_different;
use crate::{
    BuilderDirs, BuilderToast, FORMAT_FILE_NAME, TaskBuilderSettings, action_types,
    register_language,
};

/// The name of every action of the builder.
const NAMES: &[&str] = &[
    "task::Plan",
    "task::Propose",
    "task::ShowProposal",
    "task::EditProposal",
    "task::ApplyProposal",
    "task::RejectProposal",
    "task::NewWithoutPlan",
    "task::ToggleAutoApply",
    "proposal::Descend",
    "proposal::Up",
];

fn names() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut names = NAMES.to_vec();
    #[cfg(debug_assertions)]
    names.extend(["proposal::DumpView", "proposal::DumpPreview"]);
    names
}

fn test_fs_dirs() -> BuilderDirs {
    BuilderDirs::new(Path::new("/home"))
}

/// The settings the app starts with: the real `default.json`, languages included (the shared test
/// settings drop the `languages` block).
fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        let fs = FakeFs::new(cx.background_executor().clone());
        crate::init(fs, test_fs_dirs(), cx);
    });
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn proposal_files_get_the_proposal_language_and_notes_txt_plain_text(
    cx: &mut TestAppContext,
) {
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    register_language(&registry);
    for (path, expected) in [
        ("2.proposal.txt", "Proposal"),
        (
            "/home/me/.bongos/work/projects/a/proposals/12.proposal.txt",
            "Proposal",
        ),
        ("notes.txt", "Plain Text"),
        ("myproposal.txt", "Plain Text"),
    ] {
        let name = registry
            .language_for_file_path(Path::new(path))
            .map(|available| available.name().to_string());
        assert_eq!(name.as_deref(), Some(expected), "{path}");
    }
    let language = registry.language_for_name("Proposal").await.unwrap();
    assert_eq!(language.name().as_ref(), "Proposal");
    assert!(language.grammar().is_none(), "Proposal has no grammar");
}

#[gpui::test]
async fn default_settings_parse(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let settings = TaskBuilderSettings::get_global(cx);
        assert!(settings.enabled, "on by default");
        assert!(settings.auto_apply);
        assert_eq!(settings.todo_warning, 8);

        let language = LanguageName::new("Proposal");
        let proposal = AllLanguageSettings::get_global(cx).language(None, Some(&language), cx);
        assert!(!proposal.remove_trailing_whitespace_on_save);
        assert!(!proposal.ensure_final_newline_on_save);
        assert!(matches!(proposal.format_on_save, FormatOnSave::Off));
        assert!(!proposal.use_autoclose);
        assert!(!proposal.use_auto_surround);
        assert!(!proposal.auto_indent_on_paste);
        assert!(!proposal.show_completions_on_input);
        assert!(!proposal.show_edit_predictions);
        assert!(!proposal.enable_language_server);
        assert!(matches!(
            proposal.auto_indent,
            settings::AutoIndentMode::None
        ));
        assert!(matches!(
            proposal.soft_wrap,
            settings::SoftWrap::EditorWidth
        ));

        // Plain Text still rewrites.
        let plain = AllLanguageSettings::get_global(cx).language(
            None,
            Some(&LanguageName::new("Plain Text")),
            cx,
        );
        assert!(plain.remove_trailing_whitespace_on_save);
        assert!(plain.ensure_final_newline_on_save);
    });
}

/// Saves `text` to `name` through the format-on-save path, and returns the bytes on disk and the
/// language the buffer got.
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
async fn saving_a_proposal_buffer_writes_the_same_bytes(cx: &mut TestAppContext) {
    let text = "proposal 2   \n\ntask a: goal\t\n   \n  todo x  \nlast line, no newline  ";
    let (saved, language) = save_exactly(cx, "2.proposal.txt", text).await;
    assert_eq!(language, "Proposal");
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
    cx.update(crate::init_palette);

    let names = names();
    assert_eq!(names.len(), action_types().len(), "every action is listed");
    for name in &names {
        assert!(!palette_hidden(name, cx), "{name} shows by default");
    }

    set_enabled(cx, false);
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
        crate::init(app_state.fs.clone(), test_fs_dirs(), cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update_in(cx, |workspace, window, cx| {
        crate::register(workspace);
        window.focus(&gpui::Focusable::focus_handle(workspace, cx), cx)
    });
    cx.run_until_parked();
    (workspace, cx)
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
            .contains(&NotificationId::unique::<BuilderToast>())
    })
}

fn set_enabled_in(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn dispatch_by_name_shows_the_off_toast(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    set_enabled_in(cx, false);
    for name in names() {
        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<BuilderToast>(), cx)
        });
        assert!(!toast_shown(&workspace, cx));
        dispatch(name, cx);
        assert!(toast_shown(&workspace, cx), "{name} shows the off toast");
    }
}

#[test]
fn the_off_toast_names_the_setting() {
    assert_eq!(
        crate::actions::OFF_MESSAGE,
        "The task builder is off (task_builder.enabled)"
    );
}

#[gpui::test]
async fn no_toast_while_on(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_actions(cx).await;
    set_enabled_in(cx, true);
    dispatch("task::Plan", cx);
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn no_action_replaces_new_from_plan_step(cx: &mut TestAppContext) {
    cx.update(|cx| command_palette_hooks::init(cx));
    use_default_settings(cx);
    cx.update(crate::init_palette);
    set_enabled(cx, true);
    cx.update(|cx| {
        assert!(cx.build_action("task::NewFromPlanStep", None).is_err());
        // Compare the letters only, so any spelling of the command is caught.
        let squash = |name: &str| {
            name.chars()
                .filter(char::is_ascii_alphabetic)
                .collect::<String>()
                .to_lowercase()
        };
        for name in cx.all_action_names() {
            assert_ne!(squash(name), "tasknewfromplanstep", "{name}");
            assert!(!name.contains("FromPlanStep"), "{name}");
        }
    });
}

#[gpui::test]
async fn the_format_file_is_written_once_and_rewritten_only_when_different(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(cx.executor());
    let path: PathBuf = BuilderDirs::new(Path::new("/home")).format_path();
    assert_eq!(
        path,
        Path::new("/home/.bongos/task-builder").join(FORMAT_FILE_NAME)
    );
    let format = bongos_builder_model::help::FORMAT;

    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        crate::init(fs.clone(), BuilderDirs::new(Path::new("/home")), cx);
    });
    cx.run_until_parked();
    assert_eq!(fs.load(&path).await.unwrap(), format);

    // The same content writes nothing.
    assert!(
        !write_if_different(fs.as_ref(), &path, format)
            .await
            .unwrap()
    );
    let written = fs.metadata(&path).await.unwrap().unwrap().mtime;
    cx.update(|cx| {
        crate::format_file::write_at_init(fs.clone(), BuilderDirs::new(Path::new("/home")), cx)
    });
    cx.run_until_parked();
    assert_eq!(fs.metadata(&path).await.unwrap().unwrap().mtime, written);

    // A different content is replaced.
    fs.write(&path, b"edited").await.unwrap();
    cx.update(|cx| {
        crate::format_file::write_at_init(fs.clone(), BuilderDirs::new(Path::new("/home")), cx)
    });
    cx.run_until_parked();
    assert_eq!(fs.load(&path).await.unwrap(), format);
}
