//! The strip: one line at the bottom of every workspace of a window, below the status bar. It is
//! the echo area while no prompt is open and hosts the prompt that the modal layer placed at the
//! bottom (see `ModalLayer::bottom_modal`) while one is. One `Minibuffer` entity serves every
//! workspace of an OS window. It exists only while `minibuffer.enabled_for` is not empty.

use std::collections::HashMap;

use gpui::{AnyWindowHandle, App, Context, Entity, Global, Hsla, Subscription, Window};
use ui::prelude::*;
use workspace::{MinibufferMessageKind, ModalLayer, MultiWorkspace, Workspace};

use crate::MinibufferSettings;
use crate::echo::Echo;
use crate::hint::Hint;
use ::settings::Settings as _;

/// The strip of one window.
pub struct Minibuffer {
    /// The modal layer of the workspace that was active when the strip last drew, which it
    /// redraws for. The layer is drawn by the window's root, not by the workspace, so nothing
    /// else would tell the strip that a prompt opened or closed.
    observed_layer: Option<(Entity<ModalLayer>, Subscription)>,
    /// The echo area, which the strip shows while no prompt is open.
    pub(crate) echo: Echo,
    /// The pending-key hint and the prefix echo.
    pub(crate) hint: Hint,
}

/// The strip of every window that has one, keyed by window.
#[derive(Default)]
struct Minibuffers(HashMap<AnyWindowHandle, Entity<Minibuffer>>);

impl Global for Minibuffers {}

/// The strip of `window`, if it has one.
pub fn minibuffer_for_window(window: AnyWindowHandle, cx: &App) -> Option<Entity<Minibuffer>> {
    cx.try_global::<Minibuffers>()?.0.get(&window).cloned()
}

impl Minibuffer {
    fn new() -> Self {
        Self {
            observed_layer: None,
            echo: Echo::default(),
            hint: Hint::default(),
        }
    }

    fn observe(&mut self, layer: Option<Entity<ModalLayer>>, cx: &mut Context<Self>) {
        let observed = self.observed_layer.as_ref().map(|(layer, _)| layer);
        if observed == layer.as_ref() {
            return;
        }
        self.observed_layer = layer.map(|layer| {
            let subscription = cx.observe(&layer, |this, layer, cx| {
                let hosted = layer.read(cx).bottom_modal().is_some();
                this.set_hosted(hosted, cx);
                cx.notify();
            });
            (layer, subscription)
        });
    }
}

/// The colors of the strip's line, both taken from the theme: the status bar background (the
/// strip reads as part of the status area) and the border above it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StripColors {
    pub(crate) background: Hsla,
    pub(crate) border: Hsla,
}

impl StripColors {
    pub(crate) fn of(cx: &App) -> Self {
        let colors = cx.theme().colors();
        Self {
            background: colors.status_bar_background,
            border: colors.border,
        }
    }
}

impl Render for Minibuffer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layer =
            active_workspace(window, cx).map(|workspace| workspace.read(cx).modal_layer().clone());
        let hosted = layer
            .as_ref()
            .and_then(|layer| layer.read(cx).bottom_modal());
        self.observe(layer, cx);

        let line_height = window.line_height();
        let colors = StripColors::of(cx);
        let line = match hosted {
            Some((view, focus_handle)) => div()
                .id("minibuffer-hosted")
                .relative()
                .track_focus(&focus_handle)
                .child(view),
            None => {
                let echo = div().id("minibuffer-echo-area").relative();
                match (self.hint_text(), self.echo.current.as_ref()) {
                    (Some(text), _) => echo
                        .px_1()
                        .debug_selector(|| "hint-text".to_string())
                        .child(Label::new(text).single_line())
                        .children(self.hint_list(cx)),
                    (None, Some(line)) => echo
                        .px_1()
                        .debug_selector(|| "echo-text".to_string())
                        .child(Label::new(line.text.clone()).single_line().color(
                            match line.kind {
                                MinibufferMessageKind::Message => Color::Default,
                                MinibufferMessageKind::Error => Color::Error,
                            },
                        )),
                    (None, None) => echo,
                }
            }
        };
        v_flex()
            .key_context("Minibuffer")
            .flex_none()
            .w_full()
            .bg(colors.background)
            .border_t_1()
            .border_color(colors.border)
            .child(line.w_full().h(line_height))
    }
}

/// The workspace of `window` that is showing: the active one of a `MultiWorkspace`, or the
/// root itself when the window is a bare workspace.
pub(crate) fn active_workspace(window: &Window, cx: &App) -> Option<Entity<Workspace>> {
    if let Some(multi_workspace) = window.root::<MultiWorkspace>() {
        return Some(multi_workspace?.read(cx).workspace().clone());
    }
    window.root::<Workspace>().flatten()
}

/// Every workspace of `window`.
fn workspaces(window: &Window, cx: &App) -> Vec<Entity<Workspace>> {
    if let Some(multi_workspace) = window.root::<MultiWorkspace>() {
        return multi_workspace
            .map(|multi_workspace| multi_workspace.read(cx).workspaces().cloned().collect())
            .unwrap_or_default();
    }
    window.root::<Workspace>().flatten().into_iter().collect()
}

fn enabled(cx: &App) -> bool {
    !MinibufferSettings::get_global(cx).enabled_for.is_empty()
}

/// The strip of `window`, created on first use.
fn strip_for_window(window: AnyWindowHandle, cx: &mut App) -> Entity<Minibuffer> {
    if let Some(strip) = minibuffer_for_window(window, cx) {
        return strip;
    }
    let strip = cx.new(|_| Minibuffer::new());
    cx.global_mut::<Minibuffers>()
        .0
        .insert(window, strip.clone());
    strip
}

/// Installs the global and puts the strip on every workspace that opens while the minibuffer is
/// enabled.
pub(crate) fn init(cx: &mut App) {
    cx.set_global(Minibuffers::default());
    // A closed window leaves no strip behind.
    cx.on_window_closed(|cx, closed| {
        cx.global_mut::<Minibuffers>()
            .0
            .retain(|handle, _| handle.window_id() != closed);
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !enabled(cx) {
            return;
        }
        let strip = strip_for_window(window.window_handle(), cx);
        workspace.set_bottom_item(Some(strip.into()), window, cx);
    })
    .detach();
}

/// Adds the strip to, or removes it from, the workspaces of every open window, to match
/// `minibuffer.enabled_for`. Does nothing for a window that is already right.
pub(crate) fn sync_windows(cx: &mut App) {
    let enabled = enabled(cx);
    let windows = cx.windows();
    cx.global_mut::<Minibuffers>()
        .0
        .retain(|handle, _| windows.contains(handle));
    for window in windows {
        window
            .update(cx, |_, window, cx| {
                let workspaces = workspaces(window, cx);
                if workspaces.is_empty() {
                    return;
                }
                let strip = enabled.then(|| strip_for_window(window.window_handle(), cx));
                let wanted = strip.as_ref().map(|strip| strip.entity_id());
                for workspace in workspaces {
                    let current = workspace
                        .read(cx)
                        .bottom_item()
                        .map(|item| item.entity_id());
                    if current != wanted {
                        let item = strip.clone().map(Into::into);
                        workspace.update(cx, |workspace, cx| {
                            workspace.set_bottom_item(item, window, cx)
                        });
                    }
                }
            })
            .ok();
    }
    if !enabled {
        cx.global_mut::<Minibuffers>().0.clear();
    }
}
