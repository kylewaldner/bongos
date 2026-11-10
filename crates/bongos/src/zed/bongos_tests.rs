//! Promise tests: surfaces bongos removed must stay removed.

use gpui::TestAppContext;

use super::tests::init_keymap_test;

/// Namespaces that belonged to Zed's account, cloud, collaboration and hosted AI.
const REMOVED_NAMESPACES: &[&str] = &[
    "call",
    "channel",
    "collab",
    "collab_panel",
    "contacts",
    "auto_update",
    "feedback",
    "telemetry",
    "zed_predict_onboarding",
    "ai_onboarding",
    "cloud",
    "account",
    "onboarding",
    "zeta",
    "channel_modal",
    "client",
    "welcome",
];

/// Exact names of actions that were deleted.
const REMOVED_ACTIONS: &[&str] = &[
    "zed::OpenAccountSettings",
    "zed::OpenStatusPage",
    "zed::OpenOnboarding",
    "zed::ShowUpdateNotification",
    "agent::ResetTrialUpsell",
    "agent::ResetTrialEndUpsell",
    "client::SignIn",
    "client::SignOut",
    "feedback::EmailZed",
    // Guard entries: these names were removed and catch any future additions of them.
    "agent::ResetOnboarding",
    "edit_prediction::RatePredictions",
    "edit_prediction::ResetOnboarding",
    "workspace::RestoreBanner",
    "zed::OpenZedRepo",
    "zed::ShowWelcome",
    "zed::ToggleBaseKeymapSelector",
    "zed::CopySystemSpecsIntoClipboard",
    "zed::CopyInstalledExtensionsIntoClipboard",
];

/// Fragments that must not appear in any action name (lowercased, underscores removed, so they
/// match the namespace and the action part alike).
const REMOVED_WORDS: &[&str] = &[
    "zedaccount",
    "zedpredict",
    "zedpro",
    "signintozed",
    "upsell",
    "aionboarding",
    "predictonboarding",
    "collab",
    "feedback",
    "telemetry",
    "checkforupdate",
    "autoupdate",
    "requestfeature",
    "filebugreport",
    "emailzed",
    "joinchannel",
    "accountsettings",
    "statuspage",
    "openonboarding",
    "releasenotes",
];

// `zed::Extensions` and `zed::ReloadExtensions` stay on purpose: local extensions (installed and
// dev) remain usable, and nothing is fetched from the gallery, which is gone.
/// Actions that must survive: the core editor and workspace surface.
const KEPT_ACTIONS: &[&str] = &[
    "editor::Copy",
    "editor::Paste",
    "editor::Undo",
    "workspace::Save",
    "workspace::NewFile",
    "workspace::ToggleLeftDock",
    "command_palette::Toggle",
    "file_finder::Toggle",
    "zed::OpenSettings",
    "zed::Extensions",
    "zed::ReloadExtensions",
];

#[gpui::test]
async fn available_actions(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let names: Vec<&str> = cx.all_action_names().iter().copied().collect();

        let mut offenders = Vec::new();
        for name in &names {
            let namespace = name.rsplit_once("::").map_or("", |(ns, _)| ns);
            let flat = name.replace('_', "").to_lowercase();
            if REMOVED_NAMESPACES.contains(&namespace)
                || REMOVED_ACTIONS.contains(name)
                || REMOVED_WORDS.iter().any(|word| flat.contains(word))
            {
                offenders.push(*name);
            }
        }
        assert!(
            offenders.is_empty(),
            "removed actions are available again: {offenders:?}"
        );

        for kept in KEPT_ACTIONS {
            assert!(names.contains(kept), "kept action is missing: {kept}");
        }
    });
}

#[gpui::test]
async fn switch_profile_dispatches_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let switch_profile = cx
            .build_action("settings::SwitchProfile", None)
            .expect("settings::SwitchProfile is registered");
        assert!(switch_profile.partial_eq(&zed_actions::settings::SwitchProfile));

        let old_name = cx
            .build_action("settings_profile_selector::Toggle", None)
            .expect("the old name still resolves as an alias");
        assert!(old_name.partial_eq(&zed_actions::settings::SwitchProfile));
    });
}

#[gpui::test]
async fn kotlin_show_jdk_dispatches_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let show_jdk = cx
            .build_action("kotlin::ShowJdk", None)
            .expect("kotlin::ShowJdk is registered");
        assert!(show_jdk.partial_eq(&bongos_kotlin::ShowJdk));
        assert_eq!(
            command_palette::humanize_action_name("kotlin::ShowJdk"),
            "kotlin: show jdk"
        );
    });
}

#[gpui::test]
async fn kotlin_status_actions_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let cases: [(&str, Box<dyn gpui::Action>, &str); 10] = [
            (
                "kotlin::SwitchServer",
                Box::new(bongos_kotlin::SwitchServer),
                "kotlin: switch server",
            ),
            (
                "kotlin::RestartServer",
                Box::new(bongos_kotlin::RestartServer),
                "kotlin: restart server",
            ),
            (
                "kotlin::ReimportProject",
                Box::new(bongos_kotlin::ReimportProject),
                "kotlin: reimport project",
            ),
            (
                "kotlin::ShowLogs",
                Box::new(bongos_kotlin::ShowLogs),
                "kotlin: show logs",
            ),
            (
                "kotlin::ShowStatusMenu",
                Box::new(bongos_kotlin::ShowStatusMenu),
                "kotlin: show status menu",
            ),
            (
                "kotlin::CancelInstall",
                Box::new(bongos_kotlin::CancelInstall),
                "kotlin: cancel install",
            ),
            (
                "kotlin::LicenseStatus",
                Box::new(bongos_kotlin::LicenseStatus),
                "kotlin: license status",
            ),
            (
                "kotlin::LicenseActivate",
                Box::new(bongos_kotlin::LicenseActivate),
                "kotlin: license activate",
            ),
            (
                "kotlin::LicenseTrial",
                Box::new(bongos_kotlin::LicenseTrial),
                "kotlin: license trial",
            ),
            (
                "kotlin::ReviewIntellijEula",
                Box::new(bongos_kotlin::ReviewIntellijEula),
                "kotlin: review intellij eula",
            ),
        ];
        for (name, expected, humanized) in cases {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert!(action.partial_eq(expected.as_ref()), "{name}");
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        // The menu's "Open protocol log" entry builds this action by name.
        assert!(cx.build_action("dev::OpenLanguageServerLogs", None).is_ok());
    });
}

/// The `task` and `todo` actions that name a task or todo, with the palette text of each.
const WORK_ACTIONS: [(&str, &str); 16] = [
    ("task::New", "task: new"),
    ("task::NewChild", "task: new child"),
    ("task::Advance", "task: advance"),
    ("task::AdvanceOverride", "task: advance override"),
    ("task::Close", "task: close"),
    ("task::Drop", "task: drop"),
    ("task::Reopen", "task: reopen"),
    ("task::Edit", "task: edit"),
    ("todo::Add", "todo: add"),
    ("todo::Remove", "todo: remove"),
    ("todo::Skip", "todo: skip"),
    ("todo::Reopen", "todo: reopen"),
    ("todo::Move", "todo: move"),
    ("todo::SetWorkflow", "todo: set workflow"),
    ("todo::SetRole", "todo: set role"),
    ("todo::Start", "todo: start"),
];

#[gpui::test]
async fn work_task_and_todo_actions_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in WORK_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        assert!(
            cx.build_action("task::Drop", None)
                .unwrap()
                .partial_eq(&bongos_work::task::DropTask)
        );
        assert!(
            cx.build_action("todo::SetWorkflow", None)
                .unwrap()
                .partial_eq(&bongos_work::todo::SetWorkflow)
        );
    });
}

/// `task` is shared with Zed's task runner: the work actions add names and take none of its, and
/// `todo` holds only the work actions.
#[gpui::test]
async fn work_actions_share_the_task_namespace_without_colliding(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let names: Vec<&str> = cx.all_action_names().iter().copied().collect();
        let ours: Vec<&str> = WORK_ACTIONS.iter().map(|(name, _)| *name).collect();
        for kept in ["task::Spawn", "task::Rerun"] {
            assert!(names.contains(&kept), "{kept} is gone");
            assert!(!ours.contains(&kept), "{kept} is taken");
        }
        for name in &names {
            if name.starts_with("todo::") {
                assert!(ours.contains(name), "unexpected action in todo: {name}");
            }
        }
        for name in ours {
            assert_eq!(
                names.iter().filter(|existing| **existing == name).count(),
                1,
                "{name}"
            );
        }
    });
}

/// The `chat` commands of chat sessions parts 1 and 2, with the palette text of each.
const CHAT_ACTIONS: [(&str, &str); 17] = [
    ("chat::New", "chat: new"),
    ("chat::NewWithOptions", "chat: new with options"),
    ("chat::NewPlan", "chat: new plan"),
    ("chat::NewImplement", "chat: new implement"),
    ("chat::NewReview", "chat: new review"),
    ("chat::NewThinking", "chat: new thinking"),
    ("chat::NewFree", "chat: new free"),
    ("chat::Open", "chat: open"),
    ("chat::OpenAny", "chat: open any"),
    ("chat::Next", "chat: next"),
    ("chat::Previous", "chat: previous"),
    ("chat::Close", "chat: close"),
    ("chat::Archive", "chat: archive"),
    ("chat::Restore", "chat: restore"),
    ("chat::ShowArchive", "chat: show archive"),
    ("chat::Rename", "chat: rename"),
    ("chat::Delete", "chat: delete"),
];

/// The `prompts` commands, with the palette text of each.
const PROMPTS_ACTIONS: [(&str, &str); 13] = [
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
    ("prompts::ToggleAutoSubmit", "prompts: toggle auto submit"),
    ("prompts::Open", "prompts: open"),
    ("prompts::Mode", "prompts: mode"),
];

/// The `minibuffer` commands, with the palette text of each.
const MINIBUFFER_ACTIONS: [(&str, &str); 14] = [
    ("minibuffer::Cancel", "minibuffer: cancel"),
    ("minibuffer::Confirm", "minibuffer: confirm"),
    ("minibuffer::ConfirmInput", "minibuffer: confirm input"),
    ("minibuffer::SelectNext", "minibuffer: select next"),
    ("minibuffer::SelectPrev", "minibuffer: select prev"),
    ("minibuffer::PageDown", "minibuffer: page down"),
    ("minibuffer::PageUp", "minibuffer: page up"),
    (
        "minibuffer::CompleteCommonPrefix",
        "minibuffer: complete common prefix",
    ),
    ("minibuffer::HistoryPrev", "minibuffer: history prev"),
    ("minibuffer::HistoryNext", "minibuffer: history next"),
    ("minibuffer::Actions", "minibuffer: actions"),
    ("minibuffer::Open", "minibuffer: open"),
    ("minibuffer::Focus", "minibuffer: focus"),
    ("minibuffer::ShowMessages", "minibuffer: show messages"),
];

#[gpui::test]
async fn minibuffer_commands_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in MINIBUFFER_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        assert!(
            cx.build_action("minibuffer::Cancel", None)
                .unwrap()
                .partial_eq(&zed_actions::minibuffer::Cancel)
        );
    });
}

/// The `emacs` commands, with the palette text of each.
const EMACS_ACTIONS: [(&str, &str); 36] = [
    ("emacs::KeyboardQuit", "emacs: keyboard quit"),
    ("emacs::ForwardWord", "emacs: forward word"),
    ("emacs::BackwardWord", "emacs: backward word"),
    ("emacs::KillWord", "emacs: kill word"),
    ("emacs::BackwardKillWord", "emacs: backward kill word"),
    ("emacs::UpcaseWord", "emacs: upcase word"),
    ("emacs::DowncaseWord", "emacs: downcase word"),
    ("emacs::CapitalizeWord", "emacs: capitalize word"),
    ("emacs::UpcasePreviousWord", "emacs: upcase previous word"),
    (
        "emacs::DowncasePreviousWord",
        "emacs: downcase previous word",
    ),
    (
        "emacs::CapitalizePreviousWord",
        "emacs: capitalize previous word",
    ),
    ("emacs::TransposeWords", "emacs: transpose words"),
    (
        "emacs::TransposeWordsReverse",
        "emacs: transpose words reverse",
    ),
    ("emacs::TransposeChars", "emacs: transpose chars"),
    ("emacs::TransposeLines", "emacs: transpose lines"),
    ("emacs::MoveLineUp", "emacs: move line up"),
    ("emacs::MoveLineDown", "emacs: move line down"),
    ("emacs::DuplicateLine", "emacs: duplicate line"),
    ("emacs::DuplicateAndComment", "emacs: duplicate and comment"),
    ("emacs::CommentDwim", "emacs: comment dwim"),
    ("emacs::CopyFromAbove", "emacs: copy from above"),
    (
        "emacs::DeleteHorizontalSpace",
        "emacs: delete horizontal space",
    ),
    ("emacs::JustOneSpace", "emacs: just one space"),
    ("emacs::DeleteBlankLines", "emacs: delete blank lines"),
    ("emacs::ZapToChar", "emacs: zap to char"),
    ("emacs::ZapUpToChar", "emacs: zap up to char"),
    ("emacs::ZapBackToChar", "emacs: zap back to char"),
    ("emacs::ZapBackUpToChar", "emacs: zap back up to char"),
    ("emacs::KillLine", "emacs: kill line"),
    ("emacs::KillRegionOrLine", "emacs: kill region or line"),
    ("emacs::CopyRegionOrLine", "emacs: copy region or line"),
    ("emacs::Recenter", "emacs: recenter"),
    ("emacs::Reposition", "emacs: reposition"),
    ("emacs::GotoLine", "emacs: goto line"),
    ("emacs::ExpandRegion", "emacs: expand region"),
    ("emacs::ContractRegion", "emacs: contract region"),
];

#[gpui::test]
async fn emacs_commands_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in EMACS_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        let emacs_names = cx
            .all_action_names()
            .iter()
            .filter(|name| name.starts_with("emacs::"))
            .count();
        assert_eq!(emacs_names, EMACS_ACTIONS.len());

        // The count and select arguments, as feature keymaps and the prefix argument pass them.
        assert!(
            cx.build_action(
                "emacs::ForwardWord",
                Some(serde_json::json!({ "count": 3, "select": true }))
            )
            .unwrap()
            .partial_eq(&bongos_emacs::actions::ForwardWord {
                count: Some(3),
                select: true,
            })
        );
        assert!(
            cx.build_action("emacs::KillWord", Some(serde_json::json!({ "count": -2 })))
                .unwrap()
                .partial_eq(&bongos_emacs::actions::KillWord { count: Some(-2) })
        );
        assert!(
            cx.build_action(
                "emacs::ZapToChar",
                Some(serde_json::json!({ "count": 2, "char": "x" }))
            )
            .unwrap()
            .partial_eq(&bongos_emacs::actions::ZapToChar {
                count: Some(2),
                char: Some('x'),
            })
        );
        assert!(
            cx.build_action("emacs::KillWord", Some(serde_json::json!({ "cnt": 1 })))
                .is_err()
        );
    });
}

/// The word commands run on an editor when dispatched by name with their count argument, as
/// the palette and a keymap entry reach them.
#[gpui::test]
async fn emacs_word_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    use gpui::{Action as _, Focusable as _, VisualTestContext};

    init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_emacs::init(cx);
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = editor::Editor::single_line(window, cx);
        editor.set_text("one two three four", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    let run = |name: &str, args: serde_json::Value, cx: &mut VisualTestContext| {
        let action = cx.update(|_, cx| cx.build_action(name, Some(args)).unwrap());
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
    };
    let to_start = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            window.dispatch_action(
                editor::actions::MoveToBeginningOfLine::default().boxed_clone(),
                cx,
            )
        });
    };
    let text = |cx: &mut VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));

    to_start(cx);
    run("emacs::KillWord", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(text(cx), " three four");
    let clipboard = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    assert_eq!(clipboard.text().as_deref(), Some("one two"));

    to_start(cx);
    run("emacs::UpcaseWord", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(text(cx), " THREE FOUR");

    run("emacs::DowncasePreviousWord", serde_json::json!({}), cx);
    assert_eq!(text(cx), " THREE four");
}

#[gpui::test]
async fn prompts_commands_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in PROMPTS_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        assert!(
            cx.build_action("prompts::Send", None)
                .unwrap()
                .partial_eq(&bongos_prompts::prompts::Send)
        );
        assert!(
            cx.build_action("prompts::Next", None)
                .unwrap()
                .partial_eq(&bongos_prompts::prompts::Next::default())
        );
    });
}

#[gpui::test]
async fn chat_commands_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in CHAT_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        assert!(
            cx.build_action("chat::New", None)
                .unwrap()
                .partial_eq(&bongos_chat::chat::New)
        );
        assert!(
            cx.build_action("chat::OpenAny", None)
                .unwrap()
                .partial_eq(&bongos_chat::chat::OpenAny)
        );
    });
}

/// `task: mode` -> Chat finds `chat::New` by name (the Work crate does not depend on chats) and
/// dispatches it: in the app that is the real command, which answers in its own toast (here that
/// chats are off), not the Work view's "arrives with feature 04".
#[gpui::test]
async fn task_mode_chat_reaches_the_real_chat_new(cx: &mut TestAppContext) {
    use std::sync::Arc;

    use gpui::{BorrowAppContext as _, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use workspace::MultiWorkspace;
    use workspace::notifications::NotificationId;

    let app_state = super::tests::init_test(cx);
    cx.executor().allow_parking();
    let dir = std::env::temp_dir().join(format!(
        "bongos-task-mode-chat-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let root = dir.join("work");
    let project_dir = root.join("projects/invoicing");
    let write = |path: std::path::PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        project_dir.join("project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        project_dir.join("task.toml"),
        "schema = 1\nid = \"top\"\ngoal = \"Invoicing\"\nkind = \"other\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    write(
        project_dir.join("tasks/backfill/task.toml"),
        "schema = 1\nid = \"t1\"\ngoal = \"Backfill\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );

    // The task is focused, as the Work view leaves it in the key value store.
    let db = cx.update(|cx| db::kvp::KeyValueStore::global(cx));
    db.scoped(bongos_work::KVP_NAMESPACE)
        .write(bongos_work::FOCUS_KEY.to_string(), "\"t1\"".to_string())
        .await
        .unwrap();
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                content.chats.get_or_insert_default().enabled = Some(false);
            });
        });
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
        bongos_work::init(fs, bongos_work::WorkDirs { root }, cx);
        bongos_chat::init(
            bongos_chat::ChatDirs {
                types_root: dir.join("chat-types"),
                runtime: dir.join("runtime"),
            },
            cx,
        );
    });
    let loaded = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let tree =
                bongos_work::WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
            let focused = bongos_work::Focus::global(cx)
                .and_then(|focus| focus.read(cx).task_id().map(str::to_string));
            tree.is_some_and(|tree| tree.task_by_id("t1").is_some())
                && focused.as_deref() == Some("t1")
        })
    };
    for _ in 0..200 {
        cx.run_until_parked();
        if loaded(cx) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(50))
            .await;
    }
    assert!(loaded(cx), "the work tree loaded with t1 focused");

    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&gpui::Focusable::focus_handle(workspace, cx), cx)
    });
    cx.run_until_parked();
    let dispatch = |name: &str, cx: &mut VisualTestContext| {
        let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
        cx.update(|window, cx| window.dispatch_action(action, cx));
        cx.run_until_parked();
    };

    dispatch("task::Mode", cx);
    assert!(
        workspace.update_in(cx, |workspace, window, cx| workspace
            .has_active_modal(window, cx)),
        "the mode picker is open"
    );
    cx.simulate_input("Chat:");
    cx.run_until_parked();
    dispatch("menu::Confirm", cx);

    let toasts = workspace.read_with(cx, |workspace, _| workspace.notification_ids());
    assert!(
        toasts.contains(&NotificationId::unique::<bongos_chat::ChatToast>()),
        "the real chat::New answered: {toasts:?}"
    );
    assert!(
        !toasts.contains(&NotificationId::unique::<bongos_work::WorkToast>()),
        "the Work view did not say chats are missing"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A workspace in a default configuration (nothing sets `chats.enabled`) has the Chats panel.
#[gpui::test]
async fn default_workspace_has_the_chats_panel(cx: &mut TestAppContext) {
    use std::sync::Arc;

    use gpui::VisualTestContext;
    use project::Project;
    use workspace::MultiWorkspace;

    let app_state = super::tests::init_test(cx);
    cx.executor().allow_parking();
    let dir = std::env::temp_dir().join(format!(
        "bongos-default-chats-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    cx.update(|cx| {
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
        bongos_work::init(
            fs,
            bongos_work::WorkDirs {
                root: dir.join("work"),
            },
            cx,
        );
        bongos_chat::init(
            bongos_chat::ChatDirs {
                types_root: dir.join("chat-types"),
                runtime: dir.join("runtime"),
            },
            cx,
        );
    });

    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .panel::<bongos_chat::ChatPanel>(cx)
            .is_some()),
        "the Chats panel is in a workspace with default settings"
    );
    std::fs::remove_dir_all(&dir).ok();
}
