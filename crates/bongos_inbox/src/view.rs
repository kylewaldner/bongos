//! The inbox view: a workspace item titled "Inbox" that shows the list of items, one row each,
//! newest first. In the split layout the item under the cursor shows in a pane at the right of the
//! list (42% of the width below 60 em, else the list takes 34%); in the sidebar layout the view is
//! the list alone (a narrow pane or a dock panel can host it) and the item opens in a modal.
//!
//! The cursor moves with `inbox: next` and `inbox: previous` and changes nothing on disk;
//! `inbox: open item` marks the item under it read and `inbox: mark unread` unread, through the
//! inbox's queue of operations. The cursor stays on its item across rescans; when that item goes,
//! it moves to the row that followed it.
//!
//! `inbox: open item` (RET) and `inbox: focus item` (`→`) mark the item read and, in the split
//! layout, move focus to the item pane; `inbox: focus list` (`←`) and `menu: cancel` (Esc) move it
//! back. In the sidebar layout they open the modal. `inbox: open file` opens the item's file in a
//! read-only editor tab.

use crate::item::{self, ItemDetail};
use crate::modal::ItemModal;
use crate::rows::{self, PILL_FILL_OPACITY, Pill, RowKey, ViewRow};
use crate::{
    FocusItem, FocusList, Inbox, InboxEvent, InboxSettings, InboxState, MarkUnread, Next, OpenFile,
    OpenItem, Previous,
};
use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, IntoElement, MouseButton, Render, ScrollHandle, ScrollStrategy,
    SharedString, Subscription, UniformListScrollHandle, WeakEntity, Window, div, point, px,
    relative, uniform_list,
};
use language::Buffer;
use settings::Settings as _;
use std::collections::HashMap;
use std::rc::Rc;
use ui::{Label, LabelSize, prelude::*, rems_from_px};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

/// The height of the bar over the list.
const BAR_HEIGHT: f32 = 30.;
/// The height of the line along the top of the view while it has focus.
const FOCUS_LINE: f32 = 2.;
/// The size of the unread dot.
const DOT: f32 = 7.;
/// Below this width, in ems, the list takes 42% of the split instead of 34%.
const NARROW_EMS: f32 = 60.;
/// How far `inbox: next` and `inbox: previous` scroll the item pane while it has focus.
const SCROLL_STEP: f32 = 48.;

/// Where the view shows the item under the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The list and the item pane side by side.
    Split,
    /// The list alone; the item opens in a modal.
    Sidebar,
}

/// Where the inbox reads the time for ages; the system clock unless replaced.
pub struct Clock(pub Rc<dyn Fn() -> DateTime<Utc>>);

impl Global for Clock {}

/// The current time for ages, from the [`Clock`] global when one is set.
pub fn now(cx: &App) -> DateTime<Utc> {
    cx.try_global::<Clock>()
        .map_or_else(Utc::now, |clock| (clock.0)())
}

/// Replaces the clock the inbox reads ages against.
pub fn set_clock(clock: impl Fn() -> DateTime<Utc> + 'static, cx: &mut App) {
    cx.set_global(Clock(Rc::new(clock)));
}

pub struct InboxView {
    inbox: Option<Entity<InboxState>>,
    workspace: WeakEntity<Workspace>,
    layout: Layout,
    focus_handle: FocusHandle,
    /// The item pane's focus handle (split layout).
    item_focus: FocusHandle,
    item_scroll: ScrollHandle,
    scroll_handle: UniformListScrollHandle,
    rows: Vec<ViewRow>,
    /// The unread count, `None` while the inbox is off.
    unread: Option<usize>,
    cursor: usize,
    /// Items with an operation queued and the state it asks for (`true` for read), which their
    /// rows show until the operation is done.
    pending: HashMap<String, bool>,
    /// The row under the cursor, kept across rescans.
    selected: Option<RowKey>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<()> for InboxView {}

impl InboxView {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let inbox = Inbox::global(cx);
        let mut subscriptions = Vec::new();
        if let Some(inbox) = &inbox {
            subscriptions.push(cx.subscribe(inbox, |this, _, _: &InboxEvent, cx| {
                this.rebuild(cx);
            }));
            subscriptions.push(cx.observe(inbox, |this, _, cx| this.rebuild(cx)));
        }
        let mut this = Self {
            inbox,
            workspace,
            layout: Layout::Split,
            focus_handle: cx.focus_handle(),
            item_focus: cx.focus_handle(),
            item_scroll: ScrollHandle::new(),
            scroll_handle: UniformListScrollHandle::new(),
            rows: Vec::new(),
            unread: None,
            cursor: 0,
            pending: HashMap::new(),
            selected: None,
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Switches between the split layout and the list-only sidebar layout.
    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        self.layout = layout;
        cx.notify();
    }

    /// Whether the item pane has the focus.
    pub fn item_focused(&self, window: &Window) -> bool {
        self.item_focus.is_focused(window)
    }

    /// The item under the cursor as the item pane shows it; `None` on a group row or an empty list.
    pub fn detail(&self, cx: &App) -> Option<ItemDetail> {
        let (unique, inode) = self.selected()?.item.as_ref()?;
        let snapshot = self.inbox.as_ref()?.read(cx).snapshot()?;
        let (entry, parsed) = snapshot
            .items()
            .iter()
            .find(|(entry, _)| &entry.unique == unique && entry.inode == *inode)?;
        Some(ItemDetail::new(self.selected()?, entry, parsed, now(cx)))
    }

    /// The cursor's place for the item bar: `(<i>, <n>)`, `None` on an empty list.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.cursor().map(|cursor| (cursor + 1, self.rows.len()))
    }

    pub fn rows(&self) -> &[ViewRow] {
        &self.rows
    }

    /// The index of the row under the cursor, `None` when the list is empty.
    pub fn cursor(&self) -> Option<usize> {
        (!self.rows.is_empty()).then_some(self.cursor)
    }

    pub fn selected(&self) -> Option<&ViewRow> {
        self.cursor().and_then(|index| self.rows.get(index))
    }

    /// The text `inbox: dump view` writes for this view.
    pub fn dump_text(&self, cx: &App) -> String {
        rows::dump_text(self.unread, &self.rows, self.cursor(), now(cx))
    }

    /// Reads the rows from the inbox's latest snapshot and puts the cursor back on its row, or on
    /// the row that followed it when it went away.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let (unread, mut rows) = match self.inbox.as_ref().map(|inbox| inbox.read(cx)) {
            Some(inbox) if inbox.is_enabled() => match inbox.snapshot() {
                Some(snapshot) => (Some(snapshot.unread_count()), rows::view_rows(&snapshot)),
                None => (Some(0), Vec::new()),
            },
            _ => (None, Vec::new()),
        };
        for row in &mut rows {
            if let Some(read) = self
                .pending
                .get(row.item.as_ref().map_or("", |item| item.0.as_str()))
            {
                row.unread = !read;
            }
        }
        if unread == self.unread && rows == self.rows {
            // The rows are the same but their ages are not.
            cx.notify();
            return;
        }
        let old = std::mem::replace(&mut self.rows, rows);
        self.unread = unread;
        let position = |key: &RowKey| self.rows.iter().position(|row| &row.key == key);
        let kept = self.selected.as_ref().and_then(position);
        let cursor = kept
            .or_else(|| {
                // The row under the cursor went away: the first later row still there.
                let from = (self.cursor + 1).min(old.len());
                old[from..].iter().find_map(|row| position(&row.key))
            })
            .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1)));
        self.set_cursor(cursor, cx);
    }

    fn set_cursor(&mut self, index: usize, cx: &mut Context<Self>) {
        self.cursor = index.min(self.rows.len().saturating_sub(1));
        self.selected = self.rows.get(self.cursor).map(|row| row.key.clone());
        if !self.rows.is_empty() {
            self.scroll_handle
                .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Moves the cursor by `offset` rows, stopping at either end. Renames nothing.
    pub fn move_cursor(&mut self, offset: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let index = self.cursor.saturating_add_signed(offset);
        self.set_cursor(index, cx);
    }

    /// Marks the item under the cursor read. Does nothing on a read item or a group row.
    pub fn open_item(&mut self, cx: &mut Context<Self>) {
        self.operate(true, cx);
    }

    /// RET and `→`: marks the item under the cursor read and shows it, in the item pane (which
    /// takes the focus) or in the modal. Does nothing on a group row.
    pub fn focus_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected().is_none_or(|row| row.item.is_none()) {
            return;
        }
        self.operate(true, cx);
        match self.layout {
            Layout::Split => window.focus(&self.item_focus, cx),
            Layout::Sidebar => self.show_modal(window, cx),
        }
    }

    /// `←` and Esc: the focus goes back to the list.
    pub fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn show_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, cx| ItemModal::new(view, cx));
            })
            .log_err();
    }

    /// Moves the cursor by `offset` rows and marks the item there read: what `n` and `p` do in the
    /// modal.
    pub fn step_item(&mut self, offset: isize, cx: &mut Context<Self>) {
        self.move_cursor(offset, cx);
        self.operate(true, cx);
    }

    /// Opens the file of the item under the cursor in a read-only editor tab, markdown
    /// highlighted.
    pub fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.detail(cx).map(|detail| detail.path) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                let location =
                    Workspace::project_path_for_path(workspace.project().clone(), &path, false, cx);
                cx.spawn_in(window, async move |workspace, cx| {
                    let (worktree, project_path) = location.await?;
                    // A file outside the project is a worktree of its own, and its entry is there
                    // once the scan has finished.
                    let scanned = worktree.read_with(cx, |worktree, _| {
                        worktree.as_local().map(|local| local.scan_complete())
                    });
                    if let Some(scanned) = scanned {
                        scanned.await;
                    }
                    let item = workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(project_path, None, true, window, cx)
                        })?
                        .await?;
                    workspace.update_in(cx, |workspace, _, cx| {
                        let Some(editor) = item.downcast::<editor::Editor>() else {
                            return;
                        };
                        editor.update(cx, |editor, cx| editor.set_read_only(true));
                        let buffer = editor.read(cx).buffer().read(cx).as_singleton();
                        let languages = workspace.project().read(cx).languages().clone();
                        if let Some(buffer) = buffer {
                            cx.spawn(async move |_, cx| {
                                let language = languages.language_for_name("Markdown").await?;
                                buffer.update(cx, |buffer: &mut Buffer, cx| {
                                    buffer.set_language(Some(language), cx)
                                });
                                anyhow::Ok(())
                            })
                            .detach_and_log_err(cx);
                        }
                    })
                })
                .detach_and_log_err(cx);
            })
            .log_err();
    }

    /// Marks the item under the cursor unread. Does nothing on an unread item or a group row.
    pub fn mark_unread(&mut self, cx: &mut Context<Self>) {
        self.operate(false, cx);
    }

    fn operate(&mut self, read: bool, cx: &mut Context<Self>) {
        let (Some(inbox), Some(row)) = (self.inbox.clone(), self.selected()) else {
            return;
        };
        // Only a change is queued. The row turns at once, so a second key before the operation
        // has run (RET, then `u`) sees the state the first one asked for.
        let Some((unique, inode)) = row.item.clone().filter(|_| row.unread == read) else {
            return;
        };
        self.pending.insert(unique.clone(), read);
        self.rows[self.cursor].unread = !read;
        let key = unique.clone();
        let task = inbox.update(cx, |inbox, cx| {
            if read {
                inbox.mark_read(&unique, inode, cx)
            } else {
                inbox.mark_unread(&unique, inode, cx)
            }
        });
        cx.notify();
        // Whatever the operation came to, the rows are the snapshot's again afterwards.
        cx.spawn(async move |this, cx| {
            task.await;
            this.update(cx, |this, cx| {
                // A later operation on the same item has its own entry.
                if this.pending.get(&key) == Some(&read) {
                    this.pending.remove(&key);
                }
                this.rebuild(cx)
            })
            .ok();
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(inbox) = &self.inbox {
            inbox.update(cx, |inbox, _| inbox.refresh());
        }
    }

    fn render_row(
        &self,
        index: usize,
        row: &ViewRow,
        focused: bool,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let cursor = index == self.cursor;
        let tint = if focused {
            colors.ghost_element_selected
        } else {
            colors.ghost_element_hover
        };
        let summary = Label::new(row.summary.clone())
            .color(if row.unread {
                Color::Default
            } else {
                Color::Muted
            })
            .weight(if row.unread {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .single_line()
            .truncate();
        let first = h_flex()
            .gap(px(8.))
            .child(div().flex_1().min_w_0().overflow_hidden().child(summary))
            .children(row.pill.map(|pill| render_pill(pill, cx)))
            .child(
                div().flex_none().child(
                    Label::new(row.age(now))
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
            );
        h_flex()
            .id(ElementId::NamedInteger("inbox-row".into(), index as u64))
            .w_full()
            .items_start()
            .gap(px(10.))
            .pl(px(12.))
            .pr(px(14.))
            .py(px(7.))
            .when(cursor, |row| row.bg(tint))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.set_cursor(index, cx);
                window.focus(&this.focus_handle, cx);
                // In the sidebar a click opens the item.
                if this.layout == Layout::Sidebar {
                    this.focus_item(window, cx);
                }
            }))
            .child(
                div()
                    .flex_none()
                    .mt(px(7.))
                    .size(px(DOT))
                    .rounded_full()
                    .when(row.unread, |dot| dot.bg(colors.text_accent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(first)
                    .child(render_second_line(row, cx)),
            )
            .into_any_element()
    }
}

/// `project · type · source`, small and in `text.placeholder`: the project in `text.muted`, the
/// source in the buffer font. Each part truncates with an ellipsis.
fn render_second_line(row: &ViewRow, cx: &App) -> impl IntoElement {
    let part = |text: &str, color: Color, buffer: bool| {
        let label = Label::new(SharedString::from(text.to_string()))
            .size(LabelSize::Small)
            .color(color)
            .single_line()
            .truncate();
        let label = if buffer { label.buffer_font(cx) } else { label };
        div().min_w_0().overflow_hidden().child(label)
    };
    let mut line = h_flex().mt(px(1.)).gap(px(8.)).overflow_hidden();
    let parts = [
        (&row.project, Color::Muted, false),
        (&row.kind, Color::Placeholder, false),
        (&row.source, Color::Placeholder, true),
    ];
    let mut first = true;
    for (text, color, buffer) in parts {
        if text.is_empty() {
            continue;
        }
        if !first {
            line = line.child(
                div().flex_none().child(
                    Label::new("·")
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                ),
            );
        }
        first = false;
        let element = part(text, color, buffer);
        // The source gives way first, then the type; the project stays whole.
        line = line.child(if buffer {
            element.flex_1()
        } else if color == Color::Muted {
            element.flex_none()
        } else {
            element.flex_shrink_1()
        });
    }
    line
}

/// A status pill: its word in the status color on that color at 16%, fully rounded; the
/// unverified pill is a dashed outline with no fill.
fn render_pill(pill: Pill, cx: &App) -> impl IntoElement {
    let color = pill.color(cx);
    let base = div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(17.))
        .rounded_full()
        .text_color(color)
        .text_size(rems_from_px(11.))
        .child(pill.label());
    if pill.dashed() {
        base.px(px(6.))
            .border_1()
            .border_dashed()
            .border_color(color)
    } else {
        base.px(px(7.)).bg(color.opacity(PILL_FILL_OPACITY))
    }
}

impl InboxView {
    /// The list pane: the bar over the rows. `focused` says whether the list has the focus.
    fn render_list(&mut self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let count = match self.unread {
            Some(count) => format!("{count} unread"),
            None => "off".to_string(),
        };
        let bar = h_flex()
            .flex_none()
            .h(px(BAR_HEIGHT))
            .px(px(14.))
            .gap(px(10.))
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                Label::new("Inbox")
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(Label::new(count).size(LabelSize::Small).color(Color::Muted));
        let body = if self.rows.is_empty() {
            let text = if self.unread.is_some() {
                "Inbox zero."
            } else {
                "The inbox is off (inbox.enabled)."
            };
            div()
                .p(px(30.))
                .child(Label::new(text).color(Color::Placeholder))
                .into_any_element()
        } else {
            let now = now(cx);
            uniform_list(
                "inbox-rows",
                self.rows.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    range
                        .filter_map(|index| {
                            let row = this.rows.get(index)?.clone();
                            Some(this.render_row(index, &row, focused, now, cx))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(bar)
            .child(body)
            .when(focused, |pane| pane.child(focus_line(cx)))
            .into_any_element()
    }

    /// The item pane: the `Item <i> of <n>` bar over the item under the cursor.
    fn render_item_pane(
        &mut self,
        focused: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = self.detail(cx);
        let bar = item::render_item_bar(self.position(), cx);
        let content = item::render_item::<Self>(
            "inbox-item",
            detail.as_ref(),
            compact,
            &self.item_scroll,
            |this, window, cx| this.open_file(window, cx),
            cx,
        );
        v_flex()
            .id("inbox-item-pane")
            .key_context("InboxItem")
            .track_focus(&self.item_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.item_focus, cx)),
            )
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(bar)
            .child(content)
            .when(focused, |pane| pane.child(focus_line(cx)))
            .into_any_element()
    }

    /// Scrolls the item pane by `rows` steps (negative is up).
    fn scroll_item(&mut self, rows: f32) {
        let mut offset = self.item_scroll.offset();
        offset.y = (offset.y - px(SCROLL_STEP * rows)).min(px(0.));
        self.item_scroll.set_offset(point(offset.x, offset.y));
    }

    fn next(&mut self, offset: isize, window: &Window, cx: &mut Context<Self>) {
        if self.item_focus.is_focused(window) {
            self.scroll_item(offset as f32);
        } else {
            self.move_cursor(offset, cx);
            self.item_scroll.set_offset(point(px(0.), px(0.)));
        }
    }
}

/// The 2 px accent line along the top edge of the pane that has the focus.
fn focus_line(cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(FOCUS_LINE))
        .bg(cx.theme().colors().text_accent)
}

impl Render for InboxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let item_focused = self.item_focus.is_focused(window);
        let list_focused = !item_focused && self.focus_handle.contains_focused(window, cx);
        let panes = match self.layout {
            Layout::Sidebar => div()
                .size_full()
                .child(self.render_list(list_focused, cx))
                .into_any_element(),
            Layout::Split => {
                let narrow =
                    window.viewport_size().width < rems(NARROW_EMS).to_pixels(window.rem_size());
                let list = self.render_list(list_focused, cx);
                let item = self.render_item_pane(item_focused, narrow, cx);
                h_flex()
                    .size_full()
                    .child(
                        div()
                            .flex_none()
                            .h_full()
                            .w(relative(if narrow { 0.42 } else { 0.34 }))
                            .border_r_1()
                            .border_color(colors.border_variant)
                            .child(list),
                    )
                    .child(item)
                    .into_any_element()
            }
        };
        div()
            .id("inbox-view")
            .key_context("InboxView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Next, window, cx| this.next(1, window, cx)))
            .on_action(cx.listener(|this, _: &Previous, window, cx| this.next(-1, window, cx)))
            .on_action(cx.listener(|this, _: &OpenItem, window, cx| this.focus_item(window, cx)))
            .on_action(cx.listener(|this, _: &FocusItem, window, cx| this.focus_item(window, cx)))
            .on_action(cx.listener(|this, _: &FocusList, window, cx| this.focus_list(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                if this.item_focus.is_focused(window) {
                    this.focus_list(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| this.open_file(window, cx)))
            .on_action(cx.listener(|this, _: &MarkUnread, _, cx| this.mark_unread(cx)))
            .on_action(cx.listener(|this, _: &crate::Refresh, _, cx| this.refresh(cx)))
            .size_full()
            .overflow_hidden()
            .bg(colors.editor_background)
            .font_ui(cx)
            .child(panes)
    }
}

impl Focusable for InboxView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for InboxView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Inbox".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Envelope))
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// `inbox: open`: focuses the inbox view of this window, else opens one in the active pane. While
/// the inbox is off it says so in the echo area instead.
pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !InboxSettings::get_global(cx).enabled {
        bongos_minibuffer::message(window, cx, crate::OFF_MESSAGE);
        return;
    }
    let existing = workspace.items_of_type::<InboxView>(cx).next();
    if let Some(view) = existing {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let workspace_handle = cx.entity().downgrade();
    let view = cx.new(|cx| InboxView::new(workspace_handle, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}
