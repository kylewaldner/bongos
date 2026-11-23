//! Tests of the `navigation` settings, namespace, actions and the echo of an in-session action.

use std::{cell::RefCell, rc::Rc};

use bongos_emacs::prefix_arg::{Meaning, meaning};
use command_palette_hooks::CommandPaletteFilter;
use gpui::{
    BorrowAppContext as _, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::{Settings as _, SettingsStore};
use workspace::{AppState, MinibufferRouting, MultiWorkspace};

use crate::NavigationSettings;
use crate::actions::{IGNORED, IN_SESSION, READS};
use crate::registry::NO_PICKER;

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        command_palette_hooks::init(cx);
        cx.set_global(MinibufferRouting::default());
        bongos_emacs::init(cx);
        crate::init(cx);
    });
}

#[gpui::test]
async fn default_settings_parse_with_the_two_keys(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let settings = NavigationSettings::get_global(cx);
        assert!(settings.include_recent_closed);
        assert_eq!(settings.preview_max_file_size, 2_097_152);
    });
}

#[gpui::test]
async fn in_session_actions_echo_no_navigation_picker_without_a_session(cx: &mut TestAppContext) {
    init(cx);
    let messages = Rc::new(RefCell::new(Vec::<String>::new()));
    cx.update(|cx| {
        let messages = messages.clone();
        cx.update_global::<MinibufferRouting, _>(|routing, _| {
            routing.set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
        });
    });
    let params = cx.update(AppState::test);
    let project = Project::test(params.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    cx.update(|window, cx| {
        let pane = workspace.read(cx).active_pane().clone();
        pane.update(cx, |pane, cx| pane.focus_handle(cx).focus(window, cx));
    });
    cx.run_until_parked();
    macro_rules! echoes {
        ($($action:ident),*) => {$(
            messages.borrow_mut().clear();
            cx.dispatch_action(crate::actions::$action);
            assert_eq!(*messages.borrow(), [NO_PICKER], stringify!($action));
        )*};
    }
    echoes!(
        KillBuffer,
        OpenInPane,
        ToggleRegex,
        ToggleCaseSensitive,
        ToggleWholeWord,
        ScopeOpenBuffers,
        ScopeDirectory,
        ToggleReplace,
        ReplaceAll,
        ExportToMultibuffer
    );
    assert_eq!(NO_PICKER, "No navigation picker");
    assert_eq!(IN_SESSION.len() + 7, READS.len() + IGNORED.len());
}

#[gpui::test]
async fn the_navigation_namespace_is_hidden_from_the_command_palette(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let action = cx
            .build_action("navigation::SwitchBuffer", None)
            .expect("navigation::SwitchBuffer is registered");
        assert!(
            CommandPaletteFilter::try_global(cx)
                .expect("the filter is installed")
                .is_hidden(action.as_ref())
        );
    });
}

#[gpui::test]
async fn find_in_files_reads_the_prefix_argument_and_the_others_ignore_it(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        assert_eq!(READS, ["navigation::FindInFiles"]);
        assert_eq!(READS.len() + IGNORED.len(), 17);
        for name in READS {
            assert!(matches!(meaning(cx, name), Some(Meaning::Reads)), "{name}");
        }
        for name in IGNORED {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::Ignored)),
                "{name}"
            );
        }
    });
}
