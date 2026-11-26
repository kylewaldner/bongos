//! The inbox view: a workspace item titled "Inbox" that shows the list of items, one row each,
//! newest first. The view is the list pane alone, so a narrow pane or a dock panel can host it.
//!
//! The cursor moves with `inbox: next` and `inbox: previous` and changes nothing on disk;
//! `inbox: open item` marks the item under it read and `inbox: mark unread` unread, through the
//! inbox's queue of operations. The cursor stays on its item across rescans; when that item goes,
//! it moves to the row that followed it.

use crate::rows::{self, PILL_FILL_OPACITY, Pill, RowKey, ViewRow};
use crate::{Inbox, InboxEvent, InboxSettings, InboxState, MarkUnread, Next, OpenItem, Previous};
use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, IntoElement, Render, ScrollStrategy, SharedString, Subscription,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use settings::Settings as _;
use std::rc::Rc;
use ui::{Label, LabelSize, prelude::*, rems_from_px};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

/// The height of the bar over the list.
const BAR_HEIGHT: f32 = 30.;
/// The height of the line along the top of the view while it has focus.
const FOCUS_LINE: f32 = 2.;
/// The size of the unread dot.
const DOT: f32 = 7.;

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
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    rows: Vec<ViewRow>,
    /// The unread count, `None` while the inbox is off.
    unread: Option<usize>,
    cursor: usize,
    /// The row under the cursor, kept across rescans.
    selected: Option<RowKey>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<()> for InboxView {}

impl InboxView {
    pub fn new(cx: &mut Context<Self>) -> Self {
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
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            rows: Vec::new(),
            unread: None,
            cursor: 0,
            selected: None,
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
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
        let (unread, rows) = match self.inbox.as_ref().map(|inbox| inbox.read(cx)) {
            Some(inbox) if inbox.is_enabled() => match inbox.snapshot() {
                Some(snapshot) => (Some(snapshot.unread_count()), rows::view_rows(&snapshot)),
                None => (Some(0), Vec::new()),
            },
            _ => (None, Vec::new()),
        };
        if unread == self.unread && rows == self.rows {
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
                .scroll_to_item(self.cursor, ScrollStrategy::Top);
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

    /// Marks the item under the cursor unread. Does nothing on an unread item or a group row.
    pub fn mark_unread(&mut self, cx: &mut Context<Self>) {
        self.operate(false, cx);
    }

    fn operate(&mut self, read: bool, cx: &mut Context<Self>) {
        let (Some(inbox), Some(row)) = (self.inbox.clone(), self.selected()) else {
            return;
        };
        // The unread state of the row is the item's: only a change is queued.
        let Some((unique, inode)) = row.item.clone().filter(|_| row.unread == read) else {
            return;
        };
        inbox
            .update(cx, |inbox, cx| {
                if read {
                    inbox.mark_read(&unique, inode, cx)
                } else {
                    inbox.mark_unread(&unique, inode, cx)
                }
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
            element.flex_shrink()
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
        base.px(px(6.)).border_1().border_dashed().border_color(color)
    } else {
        base.px(px(7.)).bg(color.opacity(PILL_FILL_OPACITY))
    }
}

impl Render for InboxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.contains_focused(window, cx);
        let colors = cx.theme().colors();
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
            .id("inbox-view")
            .key_context("InboxView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Next, _, cx| this.move_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &Previous, _, cx| this.move_cursor(-1, cx)))
            .on_action(cx.listener(|this, _: &OpenItem, _, cx| this.open_item(cx)))
            .on_action(cx.listener(|this, _: &MarkUnread, _, cx| this.mark_unread(cx)))
            .on_action(cx.listener(|this, _: &crate::Refresh, _, cx| this.refresh(cx)))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.editor_background)
            .font_ui(cx)
            .child(bar)
            .child(body)
            .when(focused, |view| {
                view.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(FOCUS_LINE))
                        .bg(colors.text_accent),
                )
            })
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
    if let Some(view) = workspace.items_of_type::<InboxView>(cx).next() {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let view = cx.new(InboxView::new);
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}
