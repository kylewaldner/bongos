//! `inbox: open as sidebar` (debug builds): the inbox list alone, in the sidebar layout, in a panel
//! of the right dock, beside whatever the window's center shows. RET or a click on a row opens the
//! item in a modal. Nothing in the app hosts the sidebar layout yet, so this is how the smoke
//! scenario and the tests reach it.

use crate::view::{InboxView, Layout};
use crate::{InboxSettings, OFF_MESSAGE, OpenAsSidebar};
use gpui::{
    Action as _, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Window, div, px,
};
use settings::Settings as _;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// The width of the sidebar.
const WIDTH: f32 = 320.;

/// A dock panel that shows an [`InboxView`] in the sidebar layout.
pub struct InboxSidebar {
    view: Entity<InboxView>,
}

impl InboxSidebar {
    pub fn new(view: Entity<InboxView>) -> Self {
        Self { view }
    }

    /// The view the panel hosts.
    pub fn view(&self) -> &Entity<InboxView> {
        &self.view
    }
}

impl EventEmitter<PanelEvent> for InboxSidebar {}

impl Focusable for InboxSidebar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for InboxSidebar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

impl Panel for InboxSidebar {
    fn persistent_name() -> &'static str {
        "InboxSidebar"
    }

    fn panel_key() -> &'static str {
        "InboxSidebar"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(WIDTH)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        None
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        OpenAsSidebar.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        90
    }
}

/// `inbox: open as sidebar`: adds the panel to the right dock on the first call, then opens the dock
/// and focuses the list.
pub(crate) fn open_as_sidebar(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !InboxSettings::get_global(cx).enabled {
        bongos_minibuffer::message(window, cx, OFF_MESSAGE);
        return;
    }
    if workspace.panel::<InboxSidebar>(cx).is_none() {
        let workspace_handle = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let mut view = InboxView::new(workspace_handle, cx);
            view.set_layout(Layout::Sidebar, cx);
            view
        });
        let panel = cx.new(|_| InboxSidebar::new(view));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<InboxSidebar>(window, cx);
}
