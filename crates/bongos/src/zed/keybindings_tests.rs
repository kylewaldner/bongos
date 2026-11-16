//! The `settings: set keybindings` picker: what it offers, what confirming and cancelling write,
//! the retired base keymap notice's button and the VS Code keys of the outline and the project
//! symbols.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, Keymap, Keystroke, TestAppContext,
    VisualTestContext,
};
use project::Project;
use settings::{BaseKeymap, Settings as _, SettingsStore};
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::settings::SetKeybindings;

use super::keybindings::{CHOICES, MINIBUFFER_NAME, candidate_text};
use super::tests::init_keymap_test;

/// A workspace with the command installed, the settings file holding `stored`, and the prompt
/// routed to the minibuffer or not.
async fn workspace_with_settings(
    stored: &str,
    routed: bool,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Arc<AppState>, VisualTestContext) {
    let app_state = init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_minibuffer::init(cx);
        super::keybindings::init(cx);
    });
    app_state
        .fs
        .save(paths::settings_file(), &stored.into(), Default::default())
        .await
        .unwrap();
    cx.update(|cx| {
        cx.update_global(|store: &mut SettingsStore, cx| store.set_user_settings(stored, cx));
        let routes = if routed {
            MINIBUFFER_NAME.to_string()
        } else {
            "file_finder".to_string()
        };
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for = Some(vec![routes]);
            });
        });
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    (workspace, app_state, cx)
}

fn open_picker(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.dispatch_action(SetKeybindings);
    cx.run_until_parked();
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .active_modal::<bongos_minibuffer::SessionView>(cx)
            .is_some()),
        "the picker is open"
    );
}

fn press(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

async fn settings_text(app_state: &Arc<AppState>) -> String {
    app_state.fs.load(paths::settings_file()).await.unwrap()
}

use std::sync::Arc;

#[test]
fn the_picker_lists_emacs_vim_vscode_and_none_with_their_labels() {
    let listed: Vec<(&str, &str)> = CHOICES
        .iter()
        .map(|(text, label, _)| (*text, *label))
        .collect();
    assert_eq!(
        listed,
        [
            ("emacs", "Emacs"),
            ("vim", "Vim"),
            ("vscode", "VS Code"),
            ("None", "None"),
        ]
    );
}

#[test]
fn the_current_base_keymap_is_the_default_candidate() {
    for (keymap, text) in [
        (BaseKeymap::Emacs, "emacs"),
        (BaseKeymap::Vim, "vim"),
        (BaseKeymap::VSCode, "vscode"),
        (BaseKeymap::None, "None"),
    ] {
        assert_eq!(candidate_text(keymap), text);
    }
}

#[gpui::test]
async fn the_default_candidate_is_the_current_keymap(cx: &mut TestAppContext) {
    let stored = r#"{ "base_keymap": "Vim", "theme": "One Dark" }"#;
    let (workspace, app_state, mut cx) = workspace_with_settings(stored, true, cx).await;
    let cx = &mut cx;
    assert_eq!(
        cx.update(|_, cx| *BaseKeymap::get_global(cx)),
        BaseKeymap::Vim
    );
    open_picker(&workspace, cx);
    // Confirming at once answers the default, which is the keymap in use.
    press("menu::Confirm", cx);
    assert!(
        settings_text(&app_state)
            .await
            .contains(r#""base_keymap": "Vim""#)
    );
}

#[gpui::test]
async fn confirming_vim_writes_base_keymap_and_keeps_every_other_key(cx: &mut TestAppContext) {
    let stored = r#"{
    "base_keymap": "Emacs",
    "theme": "One Dark",
    "tab_size": 2
}"#;
    let (workspace, app_state, mut cx) = workspace_with_settings(stored, true, cx).await;
    let cx = &mut cx;
    open_picker(&workspace, cx);
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .modal_layer()
            .read(cx)
            .bottom_modal()
            .is_some()),
        "the picker is drawn in the strip"
    );
    cx.simulate_input("vim");
    cx.run_until_parked();
    press("menu::Confirm", cx);
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .active_modal::<bongos_minibuffer::SessionView>(cx)
            .is_none()),
        "the picker is closed"
    );
    let text = settings_text(&app_state).await;
    assert!(text.contains(r#""base_keymap": "Vim""#), "{text}");
    assert!(text.contains(r#""theme": "One Dark""#), "{text}");
    assert!(text.contains(r#""tab_size": 2"#), "{text}");
    assert_eq!(
        cx.update(|_, cx| *BaseKeymap::get_global(cx)),
        BaseKeymap::Vim
    );
}

#[gpui::test]
async fn every_candidate_writes_its_serialized_name(cx: &mut TestAppContext) {
    for (typed, written) in [
        ("emacs", "Emacs"),
        ("vim", "Vim"),
        ("vscode", "VSCode"),
        ("None", "None"),
    ] {
        let (workspace, app_state, mut cx) = workspace_with_settings("{}", true, cx).await;
        let cx = &mut cx;
        open_picker(&workspace, cx);
        cx.simulate_input(typed);
        cx.run_until_parked();
        press("menu::Confirm", cx);
        let text = settings_text(&app_state).await;
        assert!(
            text.contains(&format!(r#""base_keymap": "{written}""#)),
            "{typed}: {text}"
        );
    }
}

#[gpui::test]
async fn cancelling_writes_nothing(cx: &mut TestAppContext) {
    let stored = r#"{ "base_keymap": "Emacs", "theme": "One Dark" }"#;
    let (workspace, app_state, mut cx) = workspace_with_settings(stored, true, cx).await;
    let cx = &mut cx;
    open_picker(&workspace, cx);
    cx.simulate_input("vim");
    cx.run_until_parked();
    // C-g is bound to `menu::Cancel` in the minibuffer.
    press("menu::Cancel", cx);
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .active_modal::<bongos_minibuffer::SessionView>(cx)
            .is_none()),
        "the picker is closed"
    );
    assert_eq!(settings_text(&app_state).await, stored);
}

#[gpui::test]
async fn the_picker_is_centered_when_not_routed(cx: &mut TestAppContext) {
    let (workspace, _app_state, mut cx) = workspace_with_settings("{}", false, cx).await;
    let cx = &mut cx;
    open_picker(&workspace, cx);
    assert!(
        workspace.read_with(cx, |workspace, cx| workspace
            .modal_layer()
            .read(cx)
            .bottom_modal()
            .is_none()),
        "the picker is a centered modal"
    );
}

#[gpui::test]
async fn the_retired_keymap_notice_button_dispatches_set_keybindings(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let dispatched = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let dispatched = dispatched.clone();
        cx.on_action(move |_: &SetKeybindings, _| dispatched.set(dispatched.get() + 1));
    });
    let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
    let dismissed = Rc::new(Cell::new(false));
    // The notification stays alive, or the event is never delivered.
    let _notification = cx.update(|window, cx| {
        let notification = cx.new(|cx| MessageNotification::new("notice", cx));
        let flag = dismissed.clone();
        cx.subscribe(&notification, move |_, _: &gpui::DismissEvent, _| {
            flag.set(true)
        })
        .detach();
        notification.update(cx, |_, cx| {
            super::set_keybindings_from_notice(window, cx);
        });
        notification
    });
    cx.run_until_parked();
    assert_eq!(dispatched.get(), 1);
    assert!(dismissed.get());
}

#[gpui::test]
async fn the_vscode_keys_of_outline_and_project_symbols_resolve(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let bindings =
        cx.update(|cx| super::profile_key_bindings(BaseKeymap::VSCode, false, false, cx));
    let keymap = Keymap::new(bindings);
    let contexts: Vec<_> = ["Workspace", "Pane", "Editor mode=full"]
        .iter()
        .map(|context| gpui::KeyContext::parse(context).unwrap())
        .collect();
    for (input, action) in [
        ("ctrl-shift-o", "outline::Toggle"),
        ("ctrl-t", "project_symbols::Toggle"),
    ] {
        let (found, _) = keymap.bindings_for_input(&[Keystroke::parse(input).unwrap()], &contexts);
        assert_eq!(
            found.first().map(|b| b.action().name()),
            Some(action),
            "{input}"
        );
    }
}
