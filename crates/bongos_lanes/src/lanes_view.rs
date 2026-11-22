//! The Lanes view: the lanes of the project as a workspace item (key context `LanesView`). It
//! renders [`lanes_view_rows`]: the header ("3 of 5 lanes busy, 4 tasks ready, 6 waiting"), a row
//! per lane with the reason it is not working, and what needs a person. Sections other features
//! register ([`LanesViewSections`]) render below; each has its own [`FocusHandle`] and key
//! context, and the view's point (`n` and `p`, or a click) moves focus into the section that holds
//! the row, so the section's key bindings apply while its row is current, and back to the
//! view's own handle on a lane row.
//!
//! The rows read only the store's run state and records ([`LanesSource`]), so a window that is a
//! client of an engine host shows the lanes before any event arrives.

use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bongos_lanes_model::{DryRun, explain};
use bongos_project_repo::ProjectRepo;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div,
};
use settings::Settings as _;
use ui::{Color, Icon, IconName, Label, LabelCommon as _, LabelSize, prelude::*, v_flex};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::LanesSettings;
use crate::lane_control::{LanesStatus, status_from_read};
use crate::lanes_rows::{LanesViewRows, RowFacts, lanes_view_rows};
use crate::read_modes::load;
use crate::store::read_state;
use crate::wait_text::{WaitTextFn, WaitTextFormatters};

/// One feature's section of the Lanes view (pools, queues).
pub trait LanesViewSection {
    /// The key context of the section; its bindings apply while the point is on its rows.
    fn key_context(&self) -> &'static str;
    /// The heading of the section.
    fn title(&self) -> String;
    /// The rows of the section.
    fn rows(&self, cx: &App) -> Vec<String>;
    /// Lets the section attach its action handlers to its element.
    fn decorate(&self, element: Stateful<gpui::Div>) -> Stateful<gpui::Div> {
        element
    }
}

/// The registered sections of the Lanes view, in the order they were registered.
#[derive(Default, Clone)]
pub struct LanesViewSections {
    sections: Vec<Arc<dyn LanesViewSection>>,
}

impl Global for LanesViewSections {}

impl LanesViewSections {
    /// Registers `section`; views opened afterwards show it.
    pub fn register(section: Arc<dyn LanesViewSection>, cx: &mut App) {
        cx.default_global::<Self>().sections.push(section);
    }

    /// Registers the formatter of the text of a lane's wait (see [`WaitTextFormatters`]).
    pub fn wait_text(formatter: WaitTextFn, cx: &mut App) {
        WaitTextFormatters::register(formatter, cx);
    }

    /// Every registered section.
    pub fn sections(cx: &App) -> Vec<Arc<dyn LanesViewSection>> {
        cx.try_global::<Self>()
            .map(|sections| sections.sections.clone())
            .unwrap_or_default()
    }
}

/// Where the view reads the lanes from.
pub trait LanesSource {
    /// The lanes' status, from the run state and records.
    fn status(&self, cx: &App) -> Result<LanesStatus>;
    /// A dry run of the same moment, for the ready and waiting counts and the reason of an idle
    /// lane, when it can be had. It runs in the background.
    fn dry_run(&self, _cx: &App) -> Option<Task<Result<DryRun>>> {
        None
    }
}

/// The lanes read from a project's store: through a read handle in a client window, through the
/// writer in the window that holds it.
pub struct StoreLanes {
    pub repo: ProjectRepo,
}

impl LanesSource for StoreLanes {
    fn status(&self, _cx: &App) -> Result<LanesStatus> {
        self.repo
            .state()
            .context("the project's state is not open")?
            .read(status_from_read)
    }

    fn dry_run(&self, cx: &App) -> Option<Task<Result<DryRun>>> {
        let state = self.repo.state()?.read(read_state).ok()?;
        let location = self.repo.location().clone();
        let limit = LanesSettings::get_global(cx).dry_run_limit as usize;
        Some(cx.background_spawn(async move {
            let loaded = load(&location, limit).await?;
            Ok(explain(
                &loaded.graph,
                &state,
                &loaded.table,
                &loaded.options,
            ))
        }))
    }
}

/// A row the point can be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointRow {
    Lane(usize),
    NeedsYou(usize),
    /// Row `.1` of the section `.0`.
    Section(usize, usize),
}

struct ViewSection {
    section: Arc<dyn LanesViewSection>,
    handle: FocusHandle,
    rows: Vec<String>,
}

/// The Lanes view.
pub struct LanesView {
    source: Rc<dyn LanesSource>,
    status: Option<LanesStatus>,
    dry: Option<DryRun>,
    rows: Option<LanesViewRows>,
    error: Option<String>,
    sections: Vec<ViewSection>,
    point: usize,
    focus_handle: FocusHandle,
    _dry_task: Option<Task<()>>,
}

impl LanesView {
    pub fn new(source: Rc<dyn LanesSource>, cx: &mut Context<Self>) -> Self {
        let sections = LanesViewSections::sections(cx)
            .into_iter()
            .map(|section| ViewSection {
                section,
                handle: cx.focus_handle(),
                rows: Vec::new(),
            })
            .collect();
        let mut view = Self {
            source,
            status: None,
            dry: None,
            rows: None,
            error: None,
            sections,
            point: 0,
            focus_handle: cx.focus_handle(),
            _dry_task: None,
        };
        view.refresh(cx);
        view
    }

    /// The rows the view shows now.
    pub fn rows(&self) -> Option<&LanesViewRows> {
        self.rows.as_ref()
    }

    /// The error that kept the lanes from being read, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The handle of section `index`.
    pub fn section_handle(&self, index: usize) -> Option<FocusHandle> {
        self.sections
            .get(index)
            .map(|section| section.handle.clone())
    }

    /// Reads the lanes again (`g`). The status is read at once; the dry run that gives the ready
    /// and waiting counts follows in the background.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.source.status(cx) {
            Ok(status) => {
                self.status = Some(status);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
            }
        }
        for section in &mut self.sections {
            section.rows = section.section.rows(cx);
        }
        self.rebuild(cx);
        self._dry_task = self.source.dry_run(cx).map(|task| {
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                let dry = task.await;
                this.update(cx, |this, cx| {
                    match dry {
                        Ok(dry) => this.dry = Some(dry),
                        Err(error) => {
                            log::warn!("lanes: the dry run of the view failed: {error:#}")
                        }
                    }
                    this.rebuild(cx);
                })
                .ok();
            })
        });
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        if let Some(status) = &self.status {
            self.rows = Some(lanes_view_rows(
                status,
                &RowFacts {
                    goal: &|_| None,
                    run_lifecycle: &|_| None,
                },
                self.dry.as_ref(),
                &WaitTextFormatters::global(cx),
            ));
        }
        let len = self.point_rows().len();
        self.point = self.point.min(len.saturating_sub(1));
        cx.notify();
    }

    /// Every row the point can be on, in order: the lanes, what needs a person, then the rows
    /// of each section.
    pub fn point_rows(&self) -> Vec<PointRow> {
        let mut rows = Vec::new();
        if let Some(view) = &self.rows {
            rows.extend((0..view.rows.len()).map(PointRow::Lane));
            rows.extend((0..view.needs_you.len()).map(PointRow::NeedsYou));
        }
        for (index, section) in self.sections.iter().enumerate() {
            rows.extend((0..section.rows.len()).map(|row| PointRow::Section(index, row)));
        }
        rows
    }

    /// The row the point is on.
    pub fn point(&self) -> Option<PointRow> {
        self.point_rows().get(self.point).copied()
    }

    /// Puts the point on row `index` of [`Self::point_rows`] and moves focus: into the handle of
    /// the section that holds the row, else to the view's own.
    pub fn set_point(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.point_rows();
        if rows.is_empty() {
            return;
        }
        self.point = index.min(rows.len() - 1);
        let handle = match rows[self.point] {
            PointRow::Section(section, _) => self.sections[section].handle.clone(),
            _ => self.focus_handle.clone(),
        };
        window.focus(&handle, cx);
        cx.notify();
    }

    /// `n` and `p`: moves the point by `offset` rows.
    pub fn move_point(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.point as isize + offset).max(0) as usize;
        self.set_point(next, window, cx);
    }
}

impl EventEmitter<()> for LanesView {}

impl Focusable for LanesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LanesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let point = self.point();
        let selected = |row: PointRow| point == Some(row);
        let mut lines: Vec<gpui::AnyElement> = Vec::new();
        let mut index = 0usize;
        if let Some(rows) = &self.rows {
            lines.push(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new(rows.header.clone()).size(LabelSize::Large))
                    .child(
                        Label::new(format!(
                            "dispatcher {}{}",
                            rows.dispatcher,
                            rows.table_commit
                                .as_ref()
                                .map(|commit| format!(", lane table {commit}"))
                                .unwrap_or_default()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            );
            for (lane_index, lane) in rows.rows.iter().enumerate() {
                let mut text = format!("{}  {}  {:?}", lane.id, lane.kind, lane.state);
                if let Some(task) = &lane.task {
                    text.push_str(&format!("  {}", lane.goal.as_ref().unwrap_or(&task.0)));
                }
                if let Some(workflow) = &lane.workflow {
                    text.push_str(&format!("  {workflow}"));
                    if let Some(lifecycle) = &lane.run_lifecycle {
                        text.push_str(&format!(" ({lifecycle})"));
                    }
                }
                if let Some(reason) = &lane.reason {
                    text.push_str(&format!("  {reason}"));
                }
                let row = PointRow::Lane(lane_index);
                let at = index;
                lines.push(
                    div()
                        .id(("lanes-view-lane", lane_index))
                        .px_2()
                        .when(selected(row), |element| element.bg(colors.element_selected))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.set_point(at, window, cx)),
                        )
                        .child(Label::new(text))
                        .into_any_element(),
                );
                index += 1;
            }
            if !rows.needs_you.is_empty() {
                lines.push(
                    div()
                        .px_2()
                        .pt_2()
                        .child(Label::new("Needs you").color(Color::Muted))
                        .into_any_element(),
                );
            }
            for (ask_index, ask) in rows.needs_you.iter().enumerate() {
                let row = PointRow::NeedsYou(ask_index);
                let at = index;
                lines.push(
                    div()
                        .id(("lanes-view-ask", ask_index))
                        .px_2()
                        .when(selected(row), |element| element.bg(colors.element_selected))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.set_point(at, window, cx)),
                        )
                        .child(
                            Label::new(format!("{}  {}", ask.lane, ask.text)).color(Color::Warning),
                        )
                        .into_any_element(),
                );
                index += 1;
            }
        }
        if let Some(error) = &self.error {
            lines.push(
                div()
                    .p_2()
                    .child(Label::new(error.clone()).color(Color::Error))
                    .into_any_element(),
            );
        }
        for (section_index, section) in self.sections.iter().enumerate() {
            let mut element = div()
                .id(("lanes-view-section", section_index))
                .key_context(section.section.key_context())
                .track_focus(&section.handle)
                .px_2()
                .pt_2()
                .child(Label::new(section.section.title()).color(Color::Muted));
            for (row_index, text) in section.rows.iter().enumerate() {
                let row = PointRow::Section(section_index, row_index);
                let at = index;
                element = element.child(
                    div()
                        .id(("lanes-view-section-row", section_index * 1000 + row_index))
                        .when(selected(row), |element| element.bg(colors.element_selected))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.set_point(at, window, cx)),
                        )
                        .child(Label::new(text.clone())),
                );
                index += 1;
            }
            lines.push(section.section.decorate(element).into_any_element());
        }
        v_flex()
            .id("lanes-view")
            .key_context("LanesView")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, window, cx| {
                    this.move_point(1, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, window, cx| {
                this.move_point(-1, window, cx)
            }))
            .size_full()
            .overflow_y_scroll()
            .children(lines)
    }
}

impl Item for LanesView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Lanes".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// Shows the Lanes view of `source` in the workspace: its open view, read again, or a new item in
/// the active pane.
pub fn show_lanes_view(
    source: Rc<dyn LanesSource>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<LanesView>(cx).next();
    if let Some(view) = existing {
        view.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| LanesView::new(source, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}
