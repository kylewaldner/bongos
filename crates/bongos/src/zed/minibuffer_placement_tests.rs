//! Where each prompt declares it belongs: the five inline prompts (isearch, go to line, string
//! rectangle, zap's read-key, query replace) report `Inline`, every other prompt reports `Picker`,
//! a modal that declares nothing is a `Picker`, the `minibuffer` style puts all of them where they
//! were, and no inline prompt reads the style.

use std::time::Duration;

use bongos_minibuffer::{ReadKeyView, Session, SessionView};
use bongos_query_replace::view::QueryReplaceView;
use command_palette::CommandPalette;
use editor::{Editor, MultiBufferOffset};
use gpui::{
    AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable as _,
    IntoElement, Render, TestAppContext, VisualTestContext, WindowHandle, div,
};
use project::Project;
use settings::KeymapFile;
use workspace::{
    AppState, MinibufferHost, MinibufferRouting, MinibufferStyle, ModalView, MultiWorkspace,
    PromptPlacement, Workspace, minibuffer_host,
};
use zed_actions::minibuffer;

const HOSTS: [&str; 9] = [
    "isearch",
    "goto_line",
    "string_rectangle",
    "zap",
    "query_replace",
    "command_palette",
    "go_to_file",
    "switch_buffer",
    "placement_test_host",
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Prompt {
    Isearch,
    GotoLine,
    StringRectangle,
    Zap,
    QueryReplace,
    CommandPalette,
    GoToFile,
    SwitchBuffer,
    YesOrNo,
    String,
}

const INLINE: [Prompt; 5] = [
    Prompt::Isearch,
    Prompt::GotoLine,
    Prompt::StringRectangle,
    Prompt::Zap,
    Prompt::QueryReplace,
];
const PICKER: [Prompt; 5] = [
    Prompt::CommandPalette,
    Prompt::GoToFile,
    Prompt::SwitchBuffer,
    Prompt::YesOrNo,
    Prompt::String,
];

fn init(cx: &mut TestAppContext) -> std::sync::Arc<AppState> {
    cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_emacs::init(cx);
        bongos_isearch::init(cx);
        bongos_query_replace::init(cx);
        bongos_navigation::init(cx);
        let routing = cx.default_global::<MinibufferRouting>();
        routing.set_routes(HOSTS.iter().map(|host| host.to_string()).collect());
        routing.set_style(MinibufferStyle::Minibuffer);
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/specific-overrides.json", cx)
                .expect("the overrides keymap loads");
        cx.bind_keys(bindings);
        app_state
    })
}

/// A fresh window with an editor on two lines, the mark at 0 and the caret at 5.
async fn window(
    app_state: &std::sync::Arc<AppState>,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> = {
        let project = project.clone();
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx))
    };
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let editor = workspace.update_in(&mut cx, |workspace, window, cx| {
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer("abcdefgh\nabcdefgh\n", None, false, cx)
        });
        buffer.update(cx, |buffer, _| buffer.set_group_interval(Duration::ZERO));
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges([MultiBufferOffset(5)..MultiBufferOffset(5)])
            })
        });
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let at = snapshot.anchor_before(MultiBufferOffset(0));
        assert!(bongos_emacs::mark::set_mark(&editor, at, cx));
    });
    (workspace, editor, cx)
}

/// Opens `prompt` the way a user does, by its action where it has one.
fn open(prompt: Prompt, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    match prompt {
        Prompt::Isearch => cx.dispatch_action(bongos_isearch::actions::Forward),
        Prompt::GotoLine => cx.dispatch_action(bongos_emacs::actions::GotoLine::default()),
        Prompt::StringRectangle => cx.dispatch_action(bongos_emacs::actions::StringRectangle),
        Prompt::Zap => cx.dispatch_action(bongos_emacs::actions::ZapToChar::default()),
        Prompt::QueryReplace => {
            cx.dispatch_action(bongos_query_replace::actions::QueryReplace);
        }
        Prompt::CommandPalette => cx.dispatch_action(zed_actions::command_palette::Toggle),
        Prompt::GoToFile => cx.dispatch_action(bongos_navigation::actions::GoToFile),
        Prompt::SwitchBuffer => cx.dispatch_action(bongos_navigation::actions::SwitchBuffer),
        Prompt::YesOrNo => workspace.update_in(cx, |workspace, window, cx| {
            bongos_minibuffer::yes_or_no(
                workspace,
                "placement_test_host",
                "Sure? ",
                false,
                |_, _, _| {},
                window,
                cx,
            )
        }),
        Prompt::String => workspace.update_in(cx, |workspace, window, cx| {
            bongos_minibuffer::open(
                workspace,
                Session::string("placement_test_host", "Name: "),
                window,
                cx,
            );
        }),
    }
    cx.run_until_parked();
}

/// The placement the open modal reports, whichever kind of view it is.
fn placement(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> PromptPlacement {
    workspace.read_with(cx, |workspace, cx| {
        let mut found = Vec::new();
        if let Some(view) = workspace.active_modal::<SessionView>(cx) {
            found.push(view.read(cx).prompt_placement());
        }
        if let Some(view) = workspace.active_modal::<ReadKeyView>(cx) {
            found.push(view.read(cx).prompt_placement());
        }
        if let Some(view) = workspace.active_modal::<QueryReplaceView>(cx) {
            found.push(view.read(cx).prompt_placement());
        }
        if let Some(view) = workspace.active_modal::<CommandPalette>(cx) {
            found.push(view.read(cx).prompt_placement());
        }
        assert_eq!(found.len(), 1, "exactly one prompt is open");
        found[0]
    })
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

#[gpui::test]
async fn the_five_inline_prompts_report_inline(cx: &mut TestAppContext) {
    let app_state = init(cx);
    for prompt in INLINE {
        let (workspace, _editor, mut cx) = window(&app_state, cx).await;
        open(prompt, &workspace, &mut cx);
        assert_eq!(
            placement(&workspace, &mut cx),
            PromptPlacement::Inline,
            "{prompt:?}"
        );
        if prompt == Prompt::QueryReplace {
            // The FROM and TO prompts are inline; so is the decision view that follows them.
            cx.simulate_input("a");
            cx.dispatch_action(minibuffer::Confirm);
            cx.simulate_input("z");
            cx.dispatch_action(minibuffer::Confirm);
            cx.run_until_parked();
            let view = workspace.read_with(&cx, |workspace, cx| {
                workspace.active_modal::<QueryReplaceView>(cx)
            });
            let view = view.expect("the decision view opens");
            assert_eq!(
                view.read_with(&cx, |view, _| view.prompt_placement()),
                PromptPlacement::Inline
            );
        }
    }
}

#[gpui::test]
async fn every_other_prompt_reports_picker(cx: &mut TestAppContext) {
    let app_state = init(cx);
    for prompt in PICKER {
        let (workspace, _editor, mut cx) = window(&app_state, cx).await;
        open(prompt, &workspace, &mut cx);
        assert_eq!(
            placement(&workspace, &mut cx),
            PromptPlacement::Picker,
            "{prompt:?}"
        );
    }
}

struct Undeclared {
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for Undeclared {}

impl gpui::Focusable for Undeclared {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Undeclared {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl ModalView for Undeclared {}

#[gpui::test]
async fn a_modal_with_no_declaration_reports_picker(cx: &mut TestAppContext) {
    let app_state = init(cx);
    let (workspace, _editor, mut cx) = window(&app_state, cx).await;
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| Undeclared {
            focus: cx.focus_handle(),
        });
    });
    cx.run_until_parked();
    let view = workspace
        .read_with(&cx, |workspace, cx| {
            workspace.active_modal::<Undeclared>(cx)
        })
        .expect("the test modal is open");
    assert_eq!(
        view.read_with(&cx, |view, _| view.prompt_placement()),
        PromptPlacement::Picker
    );
}

#[gpui::test]
async fn the_minibuffer_style_opens_all_ten_where_they_opened_before(cx: &mut TestAppContext) {
    let app_state = init(cx);
    for routed in [true, false] {
        cx.update(|cx| {
            let routes = if routed {
                HOSTS.iter().map(|host| host.to_string()).collect()
            } else {
                Vec::new()
            };
            cx.default_global::<MinibufferRouting>().set_routes(routes);
        });
        for prompt in INLINE.into_iter().chain(PICKER) {
            let (workspace, _editor, mut cx) = window(&app_state, cx).await;
            open(prompt, &workspace, &mut cx);
            assert_eq!(
                in_strip(&workspace, &mut cx),
                routed,
                "{prompt:?} routed {routed}: the strip when routed, the center otherwise"
            );
            if routed {
                for host in HOSTS {
                    // The style alone decides, whatever the placement: always the strip.
                    for placement in [PromptPlacement::Picker, PromptPlacement::Inline] {
                        assert_eq!(
                            cx.update(|_, cx| minibuffer_host(host, placement, cx)),
                            Some(MinibufferHost::Strip)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_inline_prompts_never_read_the_style() {
    let files = [
        (
            "bongos_isearch/src/session.rs",
            include_str!("../../../bongos_isearch/src/session.rs"),
        ),
        (
            "bongos_emacs/src/goto_line.rs",
            include_str!("../../../bongos_emacs/src/goto_line.rs"),
        ),
        (
            "bongos_emacs/src/string_rectangle.rs",
            include_str!("../../../bongos_emacs/src/string_rectangle.rs"),
        ),
        (
            "bongos_emacs/src/zap.rs",
            include_str!("../../../bongos_emacs/src/zap.rs"),
        ),
        (
            "bongos_query_replace/src/prompts.rs",
            include_str!("../../../bongos_query_replace/src/prompts.rs"),
        ),
        (
            "bongos_query_replace/src/view.rs",
            include_str!("../../../bongos_query_replace/src/view.rs"),
        ),
    ];
    for (name, source) in files {
        for forbidden in [
            "MinibufferRouting::style",
            "MinibufferStyle",
            "routing.style()",
            ".style()",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} reads the minibuffer style: {forbidden}"
            );
        }
    }
}
