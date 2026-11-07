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

// `zed::Extensions` stays on purpose (installed and dev extensions); the gallery is gone.
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
