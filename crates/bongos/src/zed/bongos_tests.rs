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

#[gpui::test]
async fn cursor_toggle_style_dispatches_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        let action = cx
            .build_action("cursor::ToggleStyle", None)
            .expect("cursor::ToggleStyle is registered");
        assert!(action.partial_eq(&bongos_emacs::actions::ToggleStyle));
        assert_eq!(
            command_palette::humanize_action_name("cursor::ToggleStyle"),
            "cursor: toggle style"
        );
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

/// The `isearch` commands, with the palette text of each.
const ISEARCH_ACTIONS: [(&str, &str); 25] = [
    ("isearch::Forward", "isearch: forward"),
    ("isearch::Backward", "isearch: backward"),
    ("isearch::ForwardRegexp", "isearch: forward regexp"),
    ("isearch::BackwardRegexp", "isearch: backward regexp"),
    ("isearch::RepeatForward", "isearch: repeat forward"),
    ("isearch::RepeatBackward", "isearch: repeat backward"),
    ("isearch::DelChar", "isearch: del char"),
    ("isearch::Abort", "isearch: abort"),
    ("isearch::Exit", "isearch: exit"),
    ("isearch::ExitOtherEnd", "isearch: exit other end"),
    ("isearch::RingPrevious", "isearch: ring previous"),
    ("isearch::RingNext", "isearch: ring next"),
    ("isearch::YankWord", "isearch: yank word"),
    ("isearch::YankChar", "isearch: yank char"),
    ("isearch::YankLine", "isearch: yank line"),
    ("isearch::YankSymbol", "isearch: yank symbol"),
    ("isearch::YankKill", "isearch: yank kill"),
    ("isearch::QuoteNewline", "isearch: quote newline"),
    ("isearch::EditString", "isearch: edit string"),
    ("isearch::ToggleCaseFold", "isearch: toggle case fold"),
    ("isearch::ToggleRegexp", "isearch: toggle regexp"),
    (
        "isearch::ToggleLaxWhitespace",
        "isearch: toggle lax whitespace",
    ),
    ("isearch::FirstMatch", "isearch: first match"),
    ("isearch::LastMatch", "isearch: last match"),
    ("isearch::Recenter", "isearch: recenter"),
];

#[gpui::test]
async fn isearch_commands_dispatch_by_name(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for (name, humanized) in ISEARCH_ACTIONS {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            assert_eq!(action.name(), name);
            assert_eq!(command_palette::humanize_action_name(name), humanized);
        }
        let isearch_names = cx
            .all_action_names()
            .iter()
            .filter(|name| name.starts_with("isearch::"))
            .count();
        assert_eq!(isearch_names, ISEARCH_ACTIONS.len());
        for name in bongos_isearch::actions::IN_SESSION {
            assert!(
                ISEARCH_ACTIONS.iter().any(|(action, _)| action == name),
                "{name} is an isearch command"
            );
        }
        assert!(
            cx.build_action("isearch::Forward", None)
                .unwrap()
                .partial_eq(&bongos_isearch::actions::Forward)
        );
    });
}

/// The `emacs` commands, with the palette text of each.
const EMACS_ACTIONS: [(&str, &str); 44] = [
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
    ("emacs::SetMarkCommand", "emacs: set mark command"),
    (
        "emacs::ExchangePointAndMark",
        "emacs: exchange point and mark",
    ),
    ("emacs::PopMark", "emacs: pop mark"),
    ("emacs::PopGlobalMark", "emacs: pop global mark"),
    ("emacs::BeginningOfBuffer", "emacs: beginning of buffer"),
    ("emacs::EndOfBuffer", "emacs: end of buffer"),
    ("emacs::XrefFindDefinitions", "emacs: xref find definitions"),
    (
        "emacs::XrefFindImplementations",
        "emacs: xref find implementations",
    ),
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

/// An editor in a window, focused, the way a keymap entry reaches it.
async fn emacs_editor<'a>(
    text: &str,
    cx: &'a mut TestAppContext,
) -> (
    gpui::Entity<editor::Editor>,
    &'a mut gpui::VisualTestContext,
) {
    use gpui::{Focusable as _, VisualTestContext};

    init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_emacs::init(cx);
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = editor::Editor::multi_line(window, cx);
        editor.set_text(text, window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(std::time::Duration::ZERO)
        });
    });
    (editor, cx)
}

fn run_emacs(name: &str, args: serde_json::Value, cx: &mut gpui::VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, Some(args)).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn select_offsets(
    editor: &gpui::Entity<editor::Editor>,
    ranges: &[std::ops::Range<usize>],
    cx: &mut gpui::VisualTestContext,
) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(editor::SelectionEffects::no_scroll(), window, cx, |s| {
            s.select_ranges(ranges.iter().map(|range| {
                editor::MultiBufferOffset(range.start)..editor::MultiBufferOffset(range.end)
            }))
        })
    });
    cx.run_until_parked();
}

fn clipboard_text(cx: &mut gpui::VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard())
        .and_then(|item| item.text())
}

/// C-w with no region kills the line and `editor::Paste` puts it back as a line; M-w copies
/// the line and changes nothing; C-k kills to the line end. Reached by action name with a
/// count argument, with the default clipboard kill sink.
#[gpui::test]
async fn emacs_line_kills_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("one\ntwo\nthree", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));

    select_offsets(&editor, &[5..5], cx);
    run_emacs("emacs::CopyRegionOrLine", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\ntwo\nthree");
    assert_eq!(clipboard_text(cx).as_deref(), Some("two\n"));

    run_emacs("emacs::KillRegionOrLine", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\nthree");
    assert_eq!(clipboard_text(cx).as_deref(), Some("two\n"));

    // Paste restores the whole line above the caret's line, not at the caret.
    select_offsets(&editor, &[6..6], cx);
    cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Paste), cx));
    cx.run_until_parked();
    assert_eq!(text(cx), "one\ntwo\nthree");

    // The last line without a newline: the line and the newline before it go.
    select_offsets(&editor, &[12..12], cx);
    run_emacs("emacs::KillRegionOrLine", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\ntwo");
    assert_eq!(clipboard_text(cx).as_deref(), Some("three\n"));

    // C-k with a count: through two line ends; then at the end of the buffer, nothing.
    select_offsets(&editor, &[1..1], cx);
    run_emacs("emacs::KillLine", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(text(cx), "o");
    assert_eq!(clipboard_text(cx).as_deref(), Some("ne\ntwo"));
    run_emacs("emacs::KillLine", serde_json::json!({}), cx);
    assert_eq!(text(cx), "o");
    assert_eq!(clipboard_text(cx).as_deref(), Some("ne\ntwo"));
}

/// Three carets give three pieces, one undo step, and Paste at three carets restores each.
#[gpui::test]
async fn emacs_line_kills_at_three_carets_are_one_undo_step(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("aa\nbb\ncc\ndd", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));

    select_offsets(&editor, &[1..1, 4..4, 7..7], cx);
    run_emacs("emacs::KillRegionOrLine", serde_json::json!({}), cx);
    assert_eq!(text(cx), "dd");
    assert_eq!(clipboard_text(cx).as_deref(), Some("aa\nbb\ncc\n"));
    cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Undo), cx));
    cx.run_until_parked();
    assert_eq!(text(cx), "aa\nbb\ncc\ndd");
}

/// The zap commands run on an editor when dispatched by name with their count and character
/// arguments; a character that is not there leaves the text and the clipboard alone.
#[gpui::test]
async fn emacs_zap_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("foo(bar) baz(qux) end", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));

    select_offsets(&editor, &[0..0], cx);
    run_emacs(
        "emacs::ZapToChar",
        serde_json::json!({ "count": 2, "char": ")" }),
        cx,
    );
    assert_eq!(text(cx), " end");
    assert_eq!(clipboard_text(cx).as_deref(), Some("foo(bar) baz(qux)"));

    select_offsets(&editor, &[4..4], cx);
    run_emacs(
        "emacs::ZapBackUpToChar",
        serde_json::json!({ "char": "e" }),
        cx,
    );
    assert_eq!(text(cx), " e");
    assert_eq!(clipboard_text(cx).as_deref(), Some("nd"));
    run_emacs("emacs::ZapToChar", serde_json::json!({ "char": "z" }), cx);
    assert_eq!(text(cx), " e");
    assert_eq!(clipboard_text(cx).as_deref(), Some("nd"));
}

/// Recenter and reposition run on an editor when dispatched by name; a count reaches recenter
/// as an argument, and reposition leaves one caret and no region.
#[gpui::test]
async fn emacs_recenter_and_reposition_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let text = (0..100)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (editor, cx) = emacs_editor(&text, cx).await;
    let top = |cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |editor, cx| editor.scroll_position(cx).y as u32)
    };

    select_offsets(&editor, &[400..400], cx);
    let row = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.newest_display(&display).head().row().0
    });
    run_emacs("emacs::Recenter", serde_json::json!({ "count": 10 }), cx);
    assert_eq!(top(cx), row.saturating_sub(10));
    run_emacs("emacs::Recenter", serde_json::json!({}), cx);
    assert!(top(cx) < row);

    select_offsets(&editor, &[0..4, 20..30], cx);
    run_emacs("emacs::Reposition", serde_json::json!({}), cx);
    let selections = editor.update(cx, |editor, cx| {
        let display = editor.display_snapshot(cx);
        editor.selections.all_display(&display)
    });
    assert_eq!(selections.len(), 1);
    assert!(selections[0].is_empty());
}

/// Goto-line runs on an editor when dispatched by name; a count jumps with no prompt and clamps
/// to the buffer.
#[gpui::test]
async fn emacs_goto_line_runs_on_an_editor_by_name(cx: &mut TestAppContext) {
    let text = (1..=20)
        .map(|ix| format!("line {ix}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (editor, cx) = emacs_editor(&text, cx).await;
    let row = |cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            editor.selections.newest_display(&display).head().row().0
        })
    };

    run_emacs("emacs::GotoLine", serde_json::json!({ "count": 7 }), cx);
    assert_eq!(row(cx), 6);
    run_emacs("emacs::GotoLine", serde_json::json!({ "count": 500 }), cx);
    assert_eq!(row(cx), 19);
    run_emacs("emacs::GotoLine", serde_json::json!({ "count": 0 }), cx);
    assert_eq!(row(cx), 0);
}

/// The whitespace commands run on an editor when dispatched by name with their count argument,
/// as the palette and a keymap entry reach them; two carets are one undo step.
#[gpui::test]
async fn emacs_whitespace_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("a  b\nc  d\n\n\ne f", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));
    let undo = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Undo), cx));
        cx.run_until_parked();
    };

    select_offsets(&editor, &[2..2, 7..7], cx);
    run_emacs("emacs::DeleteHorizontalSpace", serde_json::json!({}), cx);
    assert_eq!(text(cx), "ab\ncd\n\n\ne f");
    undo(cx);
    assert_eq!(text(cx), "a  b\nc  d\n\n\ne f");

    select_offsets(&editor, &[2..2], cx);
    run_emacs("emacs::JustOneSpace", serde_json::json!({ "count": 3 }), cx);
    assert_eq!(text(cx), "a   b\nc  d\n\n\ne f");
    undo(cx);

    select_offsets(&editor, &[10..10], cx);
    run_emacs("emacs::DeleteBlankLines", serde_json::json!({}), cx);
    assert_eq!(text(cx), "a  b\nc  d\n\ne f");
}

/// The transpose commands run on an editor when dispatched by name with their count argument;
/// two carets are one undo step.
#[gpui::test]
async fn emacs_transpose_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("one two three\nfour five six", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));
    let undo = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Undo), cx));
        cx.run_until_parked();
    };

    select_offsets(&editor, &[7..7, 23..23], cx);
    run_emacs("emacs::TransposeWords", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one three two\nfour six five");
    undo(cx);
    assert_eq!(text(cx), "one two three\nfour five six");

    select_offsets(&editor, &[13..13, 27..27], cx);
    run_emacs("emacs::TransposeWordsReverse", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one three two\nfour six five");
    undo(cx);

    select_offsets(&editor, &[2..2, 16..16], cx);
    run_emacs(
        "emacs::TransposeChars",
        serde_json::json!({ "count": 2 }),
        cx,
    );
    assert_eq!(text(cx), "oe ntwo three\nfuro five six");
    undo(cx);
    assert_eq!(text(cx), "one two three\nfour five six");
}

/// The line commands run on an editor when dispatched by name with their count argument; a
/// moved block and a duplicate are each one undo step.
#[gpui::test]
async fn emacs_line_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("one\ntwo\nthree\nfour", cx).await;
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));
    let undo = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Undo), cx));
        cx.run_until_parked();
    };

    select_offsets(&editor, &[1..1], cx);
    run_emacs("emacs::MoveLineDown", serde_json::json!({ "count": 3 }), cx);
    assert_eq!(text(cx), "two\nthree\nfour\none");
    undo(cx);
    assert_eq!(text(cx), "one\ntwo\nthree\nfour");

    select_offsets(&editor, &[14..14], cx);
    run_emacs("emacs::MoveLineUp", serde_json::json!({ "count": 2 }), cx);
    assert_eq!(text(cx), "one\nfour\ntwo\nthree");
    undo(cx);

    select_offsets(&editor, &[5..5], cx);
    run_emacs(
        "emacs::DuplicateLine",
        serde_json::json!({ "count": 2 }),
        cx,
    );
    assert_eq!(text(cx), "one\ntwo\ntwo\ntwo\nthree\nfour");
    undo(cx);
    assert_eq!(text(cx), "one\ntwo\nthree\nfour");

    select_offsets(&editor, &[4..4], cx);
    run_emacs(
        "emacs::CopyFromAbove",
        serde_json::json!({ "count": 2 }),
        cx,
    );
    assert_eq!(text(cx), "one\nontwo\nthree\nfour");
    undo(cx);
    assert_eq!(text(cx), "one\ntwo\nthree\nfour");
}

/// The comment commands run on an editor with comment syntax when dispatched by name; each is
/// one undo step, and comment-dwim toggles.
#[gpui::test]
async fn emacs_comment_commands_run_on_an_editor_by_name(cx: &mut TestAppContext) {
    use language::{BlockCommentConfig, Language, LanguageConfig};

    let (editor, cx) = emacs_editor("one\ntwo\nthree", cx).await;
    let language = std::sync::Arc::new(Language::new(
        LanguageConfig {
            name: "Slashes".into(),
            line_comments: vec!["// ".into()],
            block_comment: Some(BlockCommentConfig {
                start: "/* ".into(),
                end: " */".into(),
                prefix: "".into(),
                tab_size: 0,
            }),
            ..Default::default()
        },
        None,
    ));
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
    });
    let text = |cx: &mut gpui::VisualTestContext| editor.update(cx, |editor, cx| editor.text(cx));
    let undo = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| window.dispatch_action(Box::new(editor::actions::Undo), cx));
        cx.run_until_parked();
    };

    select_offsets(&editor, &[5..5], cx);
    run_emacs("emacs::CommentDwim", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\n// two\nthree");
    run_emacs("emacs::CommentDwim", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\ntwo\nthree");

    select_offsets(&editor, &[1..2], cx);
    run_emacs("emacs::CommentDwim", serde_json::json!({}), cx);
    assert_eq!(text(cx), "o/* n */e\ntwo\nthree");
    undo(cx);
    assert_eq!(text(cx), "one\ntwo\nthree");

    select_offsets(&editor, &[5..5], cx);
    run_emacs("emacs::DuplicateAndComment", serde_json::json!({}), cx);
    assert_eq!(text(cx), "one\n// two\ntwo\nthree");
    undo(cx);
    assert_eq!(text(cx), "one\ntwo\nthree");
}

/// An editor that cannot be edited still copies, and C-w and C-k do nothing.
#[gpui::test]
async fn emacs_line_kills_in_a_read_only_editor(cx: &mut TestAppContext) {
    let (editor, cx) = emacs_editor("one\ntwo", cx).await;
    editor.update(cx, |editor, _| editor.set_read_only(true));
    select_offsets(&editor, &[5..5], cx);

    run_emacs("emacs::CopyRegionOrLine", serde_json::json!({}), cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("two\n"));
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("kept".into())));
    run_emacs("emacs::KillRegionOrLine", serde_json::json!({}), cx);
    run_emacs("emacs::KillLine", serde_json::json!({}), cx);
    assert_eq!(editor.update(cx, |editor, cx| editor.text(cx)), "one\ntwo");
    assert_eq!(clipboard_text(cx).as_deref(), Some("kept"));
}

/// C-w on the marker line of a sent block in a prompts buffer changes nothing: the prompts
/// guard reverts the kill, as the one transaction `apply::run` makes reaches it.
#[gpui::test]
async fn emacs_kill_on_a_sent_prompt_block_changes_nothing(cx: &mut TestAppContext) {
    use std::path::PathBuf;

    use gpui::{BorrowAppContext as _, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, OpenOptions};

    const PROMPTS: &str = "preamble\n=== prompt 2026-11-05 13:40 | sent 2026-11-05 14:02 ===\nfirst, sent\n=== prompt 2026-11-05 14:10 ===\nsecond\n";

    let app_state = init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_prompts::init(cx);
        bongos_emacs::init(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.prompts.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    app_state
        .fs
        .as_fake()
        .insert_tree("/dir", serde_json::json!({ "prompts.txt": PROMPTS }))
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    bongos_prompts::register_language(
        &project.read_with(cx, |project, _| project.languages().clone()),
    );
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update(cx, |workspace, _| bongos_prompts::register(workspace));
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                PathBuf::from("/dir/prompts.txt"),
                OpenOptions::default(),
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
    let editor = cx
        .update(|_, cx| item.act_as::<editor::Editor>(cx))
        .unwrap();
    editor.update_in(cx, |editor, window, cx| {
        use gpui::Focusable as _;
        editor.focus_handle(cx).focus(window, cx);
        let buffer = editor.buffer().read(cx).as_singleton().unwrap();
        buffer.update(cx, |buffer, _| {
            buffer.set_group_interval(std::time::Duration::ZERO)
        });
    });
    // C-w on the line above the sent block's marker, with a count that takes the marker line
    // too: the guard reverts the whole kill.
    select_offsets(&editor, &[3..3], cx);
    run_emacs(
        "emacs::KillRegionOrLine",
        serde_json::json!({ "count": 2 }),
        cx,
    );
    assert_eq!(editor.update(cx, |editor, cx| editor.text(cx)), PROMPTS);
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
        bongos_work::init(fs, bongos_work::WorkDirs::for_test(root), cx);
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
        bongos_work::init(fs, bongos_work::WorkDirs::for_test(dir.join("work")), cx);
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

/// C-g in an idle full editor runs no other action: the quit propagates, and with Zed's
/// `go_to_line::Toggle` binding unbound (as the Emacs keymap does) nothing else is reached.
#[gpui::test]
async fn emacs_idle_quit_in_a_full_editor_runs_no_other_action(cx: &mut TestAppContext) {
    use gpui::KeyBinding;

    let (editor, cx) = emacs_editor("one two", cx).await;
    let actions = std::rc::Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
    let seen = actions.clone();
    cx.update(|_, cx| {
        let seen = seen.clone();
        cx.observe_keystrokes(move |event, _, _| {
            if let Some(action) = &event.action {
                seen.borrow_mut().push(action.name());
            }
        })
        .detach();
        cx.bind_keys([
            KeyBinding::new(
                "ctrl-g",
                editor::actions::ToggleGoToLine,
                Some("Editor && mode == full"),
            ),
            KeyBinding::new(
                "ctrl-g",
                gpui::Unbind("go_to_line::Toggle".into()),
                Some("Editor"),
            ),
            KeyBinding::new(
                "ctrl-g",
                bongos_emacs::actions::KeyboardQuit,
                Some("Editor"),
            ),
        ]);
    });
    let toggled = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = toggled.clone();
    editor.update(cx, |editor, _| {
        editor
            .register_action(move |_: &editor::actions::ToggleGoToLine, _, _| flag.set(true))
            .detach();
    });
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert!(!toggled.get());
    assert!(
        !actions.borrow().contains(&"go_to_line::Toggle"),
        "ran {:?}",
        actions.borrow()
    );
    editor.update(cx, |editor, cx| assert_eq!(editor.text(cx), "one two"));
}

struct QuitPickerDelegate {
    dismissed: std::rc::Rc<std::cell::Cell<bool>>,
}

impl picker::PickerDelegate for QuitPickerDelegate {
    type ListItem = ui::ListItem;

    fn name() -> &'static str {
        "quit_test"
    }

    fn match_count(&self) -> usize {
        0
    }

    fn selected_index(&self) -> usize {
        0
    }

    fn set_selected_index(
        &mut self,
        _: usize,
        _: &mut gpui::Window,
        _: &mut gpui::Context<picker::Picker<Self>>,
    ) {
    }

    fn placeholder_text(&self, _: &mut gpui::Window, _: &mut gpui::App) -> std::sync::Arc<str> {
        "Test".into()
    }

    fn update_matches(
        &mut self,
        _: String,
        _: &mut gpui::Window,
        _: &mut gpui::Context<picker::Picker<Self>>,
    ) -> gpui::Task<()> {
        gpui::Task::ready(())
    }

    fn confirm(
        &mut self,
        _: bool,
        _: &mut gpui::Window,
        _: &mut gpui::Context<picker::Picker<Self>>,
    ) {
    }

    fn dismissed(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<picker::Picker<Self>>) {
        self.dismissed.set(true);
    }

    fn render_match(
        &self,
        _: usize,
        _: bool,
        _: &mut gpui::Window,
        _: &mut gpui::Context<picker::Picker<Self>>,
    ) -> Option<Self::ListItem> {
        None
    }
}

struct QuitPickerModal {
    picker: gpui::Entity<picker::Picker<QuitPickerDelegate>>,
}

impl gpui::EventEmitter<gpui::DismissEvent> for QuitPickerModal {}
impl workspace::ModalView for QuitPickerModal {}
impl gpui::Focusable for QuitPickerModal {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl gpui::Render for QuitPickerModal {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        self.picker.clone()
    }
}

/// C-g in a centered picker's query editor is not stolen by the quit handlers: it reaches
/// `menu::Cancel`, which runs the delegate's `dismissed` (a direct `hide_modal` would skip it).
#[gpui::test]
async fn emacs_quit_in_a_picker_query_editor_closes_it_through_cancel(cx: &mut TestAppContext) {
    use gpui::{AppContext as _, KeyBinding};
    use workspace::{AppState, MultiWorkspace};

    let app_state = init_keymap_test(cx);
    let _: &std::sync::Arc<AppState> = &app_state;
    cx.update(|cx| {
        editor::init(cx);
        bongos_emacs::init(cx);
        cx.bind_keys([
            KeyBinding::new(
                "ctrl-g",
                bongos_emacs::actions::KeyboardQuit,
                Some("Workspace"),
            ),
            KeyBinding::new("ctrl-g", menu::Cancel, None),
            KeyBinding::new(
                "ctrl-g",
                bongos_emacs::actions::KeyboardQuit,
                Some("Editor"),
            ),
        ]);
    });
    let project = project::Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    let dismissed = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = dismissed.clone();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, move |window, cx| {
            let picker = cx.new(|cx| {
                picker::Picker::uniform_list(QuitPickerDelegate { dismissed: flag }, window, cx)
            });
            cx.subscribe(&picker, |_, _, _: &gpui::DismissEvent, cx| {
                cx.emit(gpui::DismissEvent)
            })
            .detach();
            QuitPickerModal { picker }
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = (window, cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(workspace.has_active_modal(window, cx));
        assert!(workspace.focus_in_modal(window, cx));
    });
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert!(dismissed.get());
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(!workspace.has_active_modal(window, cx));
    });
}
