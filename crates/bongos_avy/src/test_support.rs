//! Helpers for tests of jump candidates: a workspace window of a fixed size, full editors with
//! fixed text in panes and in a dock, and stand-ins for the things a jump must skip (a results
//! view that acts as an editor without being one, a panel without an editor). Panes are split
//! and terminals opened with the window commands' helpers.

use std::any::TypeId;

use editor::{Editor, MultiBuffer, SelectionEffects};
use gpui::{
    Action as _, AnyEntity, App, AppContext as _, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, IntoElement, NoAction, ParentElement as _, Pixels, Render,
    SharedString, Size, Styled as _, TestAppContext, VisualTestContext, Window, WindowHandle, div,
    px,
};
use project::Project;
use workspace::{
    AppState, MultiWorkspace, Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::Item,
};

use crate::visible::{PaneText, panes};

/// The size of every test window, so layouts never depend on the machine.
pub const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1600.),
    height: px(600.),
};

/// A workspace in a window of [`WINDOW_SIZE`], with the editor and theme initialized.
pub async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let params = cx.update(AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let project = Project::test(params.fs.clone(), [], cx).await;
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
    cx.simulate_resize(WINDOW_SIZE);
    paint(cx);
    (workspace, cx)
}

/// `setup` in a multi-workspace window, as the application has, running `before` after the editor
/// is initialized and before the window opens, so what registers on every new workspace takes
/// part and its actions are dispatchable.
pub async fn setup_with(
    cx: &mut TestAppContext,
    before: impl FnOnce(&mut App),
) -> (Entity<Workspace>, VisualTestContext) {
    let params = cx.update(AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        before(cx);
    });
    let project = Project::test(params.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.simulate_resize(WINDOW_SIZE);
    paint(&mut cx);
    (workspace, cx)
}

/// Paints the window again and lets every pending task finish, so each visible editor has the
/// layout of the current state.
pub fn paint(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// The visible text of the window, as a jump would collect it now.
pub fn visible(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<PaneText> {
    cx.update(|window, cx| workspace.update(cx, |workspace, cx| panes(workspace, window, cx)))
}

/// The entity ids of the editors in `panes`, in order.
pub fn editor_ids(panes: &[PaneText]) -> Vec<EntityId> {
    panes.iter().map(|pane| pane.editor.entity_id()).collect()
}

/// Opens a full editor of `text` in `pane`, as its active item, with the cursor at the start.
pub fn add_text_editor(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let buffer = MultiBuffer::build_simple(text, cx);
    add_buffer_editor(workspace, pane, buffer, window, cx)
}

/// Opens a full editor of `buffer` in `pane`, as its active item.
pub fn add_buffer_editor(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    buffer: Entity<MultiBuffer>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let editor = cx.new(|cx| Editor::for_multibuffer(buffer, None, window, cx));
    workspace.update(cx, |workspace, cx| {
        workspace.add_item(
            pane.clone(),
            Box::new(editor.clone()),
            None,
            true,
            true,
            window,
            cx,
        );
    });
    editor
}

/// Puts the cursor of `editor` at buffer offset `offset`.
pub fn move_cursor(editor: &Entity<Editor>, offset: usize, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([
                editor::MultiBufferOffset(offset)..editor::MultiBufferOffset(offset)
            ]);
        });
    });
}

/// A results view, like project search's, that acts as an editor (`act_as::<Editor>` finds its
/// results editor) without being one.
pub struct SearchResults {
    pub editor: Entity<Editor>,
    focus_handle: FocusHandle,
}

impl EventEmitter<()> for SearchResults {}

impl Focusable for SearchResults {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SearchResults {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

impl Item for SearchResults {
    type Event = ();

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Search results".into()
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }
}

/// Opens a [`SearchResults`] view of `text` in `pane`, as its active item.
pub fn add_search_results(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SearchResults> {
    let buffer = MultiBuffer::build_simple(text, cx);
    let editor = cx.new(|cx| Editor::for_multibuffer(buffer, None, window, cx));
    let results = cx.new(|cx| SearchResults {
        editor,
        focus_handle: cx.focus_handle(),
    });
    workspace.update(cx, |workspace, cx| {
        workspace.add_item(
            pane.clone(),
            Box::new(results.clone()),
            None,
            true,
            true,
            window,
            cx,
        );
    });
    results
}

/// A dock panel that shows its pane, so an editor in it is painted as in a real dock.
pub struct EditorPanel {
    position: DockPosition,
    pane: Entity<Pane>,
    focus_handle: FocusHandle,
}

impl EventEmitter<PanelEvent> for EditorPanel {}

impl Focusable for EditorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.pane.clone())
    }
}

impl Panel for EditorPanel {
    fn persistent_name() -> &'static str {
        "AvyTestEditorPanel"
    }

    fn panel_key() -> &'static str {
        "AvyTestEditorPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, _: &mut Context<Self>) {
        self.position = position;
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<ui::IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        None
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        NoAction.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        90
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.pane.clone())
    }
}

/// Opens a dock at `position` whose panel shows a new pane with a full editor of `text`.
pub fn add_dock_editor(
    workspace: &Entity<Workspace>,
    position: DockPosition,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<Pane>, Entity<Editor>) {
    let pane = bongos_window::test_support::new_dock_pane(workspace, window, cx);
    let buffer = MultiBuffer::build_simple(text, cx);
    let editor = cx.new(|cx| Editor::for_multibuffer(buffer, None, window, cx));
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(editor.clone()), true, false, None, window, cx);
    });
    workspace.update(cx, |workspace, cx| {
        let panel = cx.new(|cx| EditorPanel {
            position,
            pane: pane.clone(),
            focus_handle: cx.focus_handle(),
        });
        workspace.add_panel(panel, window, cx);
        workspace.toggle_dock(position, window, cx);
    });
    (pane, editor)
}

/// Opens a dock at `position` with a panel that shows no editor, as the project panel.
pub fn add_project_panel(
    workspace: &Entity<Workspace>,
    position: DockPosition,
    window: &mut Window,
    cx: &mut App,
) {
    bongos_window::test_support::add_panel(workspace, position, 10, None, true, window, cx);
}
