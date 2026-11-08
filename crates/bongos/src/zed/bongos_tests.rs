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
