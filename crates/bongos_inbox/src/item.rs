//! The open item as the item pane and the modal show it: the meta line, the title, the body (with
//! a fenced diff as its own block) and the action line. [`ItemDetail`] is the parsed item without
//! any rendering, so a test reads it; [`render_item`] draws it for the pane and the modal alike.

use crate::rows::{PILL_FILL_OPACITY, Pill, ViewRow};
use bongos_inbox_model::maildir::{Entry, ParseResult};
use bongos_inbox_model::view_model::{self, DISMISS, OPEN};
use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Context, FontWeight, HighlightStyle, IntoElement, ScrollHandle, StyledText,
    Window, div, px,
};
use std::ops::Range;
use std::path::PathBuf;
use ui::{Label, LabelSize, prelude::*, rems_from_px};

/// The reading column of the item, in ems.
const COLUMN_EMS: f32 = 34.;

/// What one line of a fenced block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    Context,
}

/// A paragraph of the body: its text without the `**` marks, and the ranges that were bold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Paragraph {
    pub text: String,
    pub bold: Vec<Range<usize>>,
}

/// A part of the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Paragraph),
    /// A fenced block. The lines of a `diff` fence are colored by their first character.
    Fence {
        lang: String,
        lines: Vec<(LineKind, String)>,
    },
}

/// One entry of the action line: a key cap and its label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionCap {
    pub name: String,
    /// The key that runs the action in the inbox view, empty when none does.
    pub key: String,
    pub label: String,
}

/// What the item pane shows for the row under the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemDetail {
    pub project: String,
    pub kind: String,
    pub source: String,
    /// `<age> ago`, `just now` under a minute.
    pub age: String,
    pub pill: Option<Pill>,
    pub title: String,
    pub blocks: Vec<Block>,
    /// Why the file could not be read, for an unreadable item.
    pub unreadable: Option<String>,
    /// Whether only the header was read (the file is over the size limit).
    pub truncated: bool,
    pub actions: Vec<ActionCap>,
    /// The file `open` opens.
    pub path: PathBuf,
}

impl ItemDetail {
    pub fn new(
        row: &ViewRow,
        entry: &Entry,
        parsed: &ParseResult,
        now: DateTime<Utc>,
        known: &dyn Fn(&str) -> bool,
    ) -> Self {
        let (blocks, unreadable, truncated, names) = match parsed.as_ref() {
            Ok(item) => (
                parse_body(&item.body),
                None,
                !item.body_loaded,
                // The registered actions among the item's, then `open` and `dismiss`.
                view_model::resolve_actions(&item.actions, known),
            ),
            Err(error) => (
                Vec::new(),
                Some(error.to_string()),
                false,
                vec![OPEN.to_string(), DISMISS.to_string()],
            ),
        };
        let age = match row.age(now).as_str() {
            "now" => "just now".to_string(),
            age => format!("{age} ago"),
        };
        let mut actions: Vec<ActionCap> = names.iter().map(|name| action_cap(name)).collect();
        // `a` runs the first registered action the item lists, so only that one shows the cap.
        let listed: &[String] = match parsed.as_ref() {
            Ok(item) => &item.actions,
            Err(_) => &[],
        };
        if let Some(primary) = view_model::primary_action(listed, known) {
            if let Some(cap) = actions.iter_mut().find(|cap| cap.name == primary) {
                cap.key = "a".into();
            }
        }
        actions.push(ActionCap {
            name: "mark-unread".into(),
            key: "u".into(),
            label: "Mark unread".into(),
        });
        Self {
            project: row.project.clone(),
            kind: row.kind.clone(),
            source: row.source.clone(),
            age,
            pill: row.pill,
            title: row.summary.clone(),
            blocks,
            unreadable,
            truncated,
            actions,
            path: entry.path.clone(),
        }
    }
}

/// The label of an action name. The key is empty: a cap is shown only for a key that really runs
/// the action (see [`ActionCap::key`]).
fn action_cap(name: &str) -> ActionCap {
    let label = match name {
        DISMISS => "Dismiss".to_string(),
        OPEN => "Open file".to_string(),
        other => {
            let mut label = other.replace('-', " ");
            if let Some(first) = label.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            label
        }
    };
    let key = match name {
        DISMISS => "d",
        OPEN => "f",
        _ => "",
    };
    ActionCap {
        name: name.to_string(),
        key: key.to_string(),
        label,
    }
}

/// Splits `body` into paragraphs (separated by blank lines) and fenced blocks.
pub fn parse_body(body: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut fence: Option<(String, Vec<(LineKind, String)>)> = None;
    let flush = |paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(bold_spans(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some((lang, lines)) = fence.as_mut() {
            if trimmed.starts_with("```") {
                blocks.push(Block::Fence {
                    lang: std::mem::take(lang),
                    lines: std::mem::take(lines),
                });
                fence = None;
            } else {
                let kind = match (lang.as_str(), line.chars().next()) {
                    ("diff", Some('+')) => LineKind::Added,
                    ("diff", Some('-')) => LineKind::Removed,
                    _ => LineKind::Context,
                };
                lines.push((kind, line.to_string()));
            }
        } else if let Some(lang) = trimmed.strip_prefix("```") {
            flush(&mut paragraph, &mut blocks);
            fence = Some((lang.trim().to_string(), Vec::new()));
        } else if line.trim().is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else {
            paragraph.push(line.trim());
        }
    }
    flush(&mut paragraph, &mut blocks);
    if let Some((lang, lines)) = fence {
        // An unclosed fence runs to the end of the body.
        blocks.push(Block::Fence { lang, lines });
    }
    blocks
}

/// `source` without its `**` marks, and the ranges they enclosed.
fn bold_spans(source: &str) -> Paragraph {
    let mut text = String::new();
    let mut bold = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("**") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("**").filter(|end| *end > 0) else {
            break;
        };
        text.push_str(&rest[..start]);
        let from = text.len();
        text.push_str(&after[..end]);
        bold.push(from..text.len());
        rest = &after[end + 2..];
    }
    text.push_str(rest);
    Paragraph { text, bold }
}

/// The item pane's and the modal's content: the item `detail` in one reading column, scrolling in
/// `scroll`. `compact` is the half-screen form (less padding, a smaller title). With no detail it
/// reads `Nothing selected.`. `open` runs when the `open` key cap is clicked.
pub fn render_item<V: 'static>(
    id: &'static str,
    detail: Option<&ItemDetail>,
    compact: bool,
    scroll: &ScrollHandle,
    open: impl Fn(&mut V, &mut Window, &mut Context<V>) + Clone + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let Some(detail) = detail else {
        return div()
            .p(px(30.))
            .child(Label::new("Nothing selected.").color(Color::Placeholder))
            .into_any_element();
    };
    let colors = cx.theme().colors().clone();
    let (pad_y, pad_x, title_size) = if compact {
        (14., 16., 17.)
    } else {
        (18., 26., 19.)
    };

    let chip = |text: &str, project: bool| {
        let chip = div()
            .flex_none()
            .h(px(17.))
            .flex()
            .items_center()
            .px(px(7.))
            .rounded(px(3.))
            .border_1()
            .text_size(rems_from_px(12.))
            .child(SharedString::from(text.to_string()));
        if project {
            chip.text_color(colors.text)
                .bg(colors.ghost_element_hover)
                .border_color(colors.border_variant)
        } else {
            chip.text_color(colors.text_muted)
                .border_color(colors.border)
        }
    };
    let mut meta = h_flex()
        .flex_wrap()
        .items_center()
        .gap(px(10.))
        .text_size(rems_from_px(13.))
        .text_color(colors.text_muted);
    if !detail.project.is_empty() {
        meta = meta.child(chip(&detail.project, true));
    }
    meta = meta.child(chip(&detail.kind, false));
    if !detail.source.is_empty() {
        meta = meta.child(
            div()
                .font_buffer(cx)
                .child(SharedString::from(detail.source.clone())),
        );
    }
    meta = meta.child(SharedString::from(detail.age.clone()));
    if let Some(pill) = detail.pill {
        meta = meta.child(render_item_pill(pill, cx));
    }

    let title = div()
        .mt(px(4.))
        .mb(px(10.))
        .text_size(rems_from_px(title_size))
        .line_height(relative(1.25))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.text)
        .child(SharedString::from(detail.title.clone()));

    let mut body = v_flex()
        .mt(px(14.))
        .text_color(colors.editor_foreground)
        .line_height(relative(1.55));
    if let Some(reason) = &detail.unreadable {
        body = body.child(div().mb(px(12.)).child(SharedString::from(format!(
            "This item cannot be read: {reason}"
        ))));
    }
    if detail.truncated {
        body = body.child(div().mb(px(12.)).child(
            "The file is over 1 MiB, so only its header was read. Open the file to read the rest.",
        ));
    }
    for block in &detail.blocks {
        body = body.child(match block {
            Block::Paragraph(paragraph) => {
                let highlights = paragraph.bold.iter().map(|range| {
                    (
                        range.clone(),
                        HighlightStyle {
                            font_weight: Some(FontWeight::BOLD),
                            ..Default::default()
                        },
                    )
                });
                div()
                    .flex_none()
                    .mb(px(12.))
                    .child(
                        StyledText::new(SharedString::from(paragraph.text.clone()))
                            .with_highlights(highlights),
                    )
                    .into_any_element()
            }
            Block::Fence { lines, .. } => v_flex()
                .mt(px(4.))
                .mb(px(14.))
                .px(px(12.))
                .py(px(10.))
                .border_1()
                .border_color(colors.border_variant)
                .rounded(px(6.))
                .bg(colors.panel_background)
                .font_buffer(cx)
                .text_size(rems_from_px(13.))
                .line_height(relative(1.5))
                .overflow_hidden()
                .children(lines.iter().map(|(kind, line)| {
                    let color = match kind {
                        LineKind::Added => cx.theme().status().success,
                        LineKind::Removed => cx.theme().status().error,
                        LineKind::Context => colors.text_muted,
                    };
                    div()
                        .text_color(color)
                        .child(SharedString::from(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.clone()
                        }))
                }))
                .into_any_element(),
        });
    }

    let mut actions = h_flex()
        .mt(px(16.))
        .flex_wrap()
        .gap_x(px(18.))
        .gap_y(px(8.))
        .text_size(rems_from_px(13.))
        .text_color(colors.text_muted);
    for (index, action) in detail.actions.iter().enumerate() {
        let entry = h_flex()
            .id(("inbox-action", index))
            .gap(px(6.))
            .when(!action.key.is_empty(), |entry| {
                entry.child(
                    div()
                        .font_buffer(cx)
                        .text_size(rems_from_px(12.))
                        .px(px(5.))
                        .border_1()
                        .border_color(colors.border)
                        .rounded(px(3.))
                        .bg(colors.panel_background)
                        .text_color(colors.text)
                        .child(SharedString::from(action.key.clone())),
                )
            })
            .child(SharedString::from(action.label.clone()));
        actions = actions.child(if action.name == OPEN {
            let open = open.clone();
            entry
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| open(this, window, cx)))
                .into_any_element()
        } else {
            entry.into_any_element()
        });
    }

    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .px(px(pad_x))
        .py(px(pad_y))
        .child(
            v_flex()
                // A definite width (the reading column, or the pane when that is narrower): with
                // only a `max_w` the last wrapped paragraph was measured too short and the action
                // line was drawn over its last line.
                .w(rems(COLUMN_EMS))
                .max_w_full()
                .child(meta)
                .child(title)
                .child(body)
                .child(actions),
        )
        .into_any_element()
}

/// The pill of the meta line: the same form as the list's.
fn render_item_pill(pill: Pill, cx: &App) -> impl IntoElement {
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

/// The `Item  <i> of <n>` bar over the item pane and the modal.
pub fn render_item_bar(position: Option<(usize, usize)>, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    h_flex()
        .flex_none()
        .h(px(30.))
        .px(px(14.))
        .gap(px(10.))
        .border_b_1()
        .border_color(colors.border_variant)
        .child(
            Label::new("Item")
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
        )
        .children(position.map(|(index, total)| {
            Label::new(format!("{index} of {total}"))
                .size(LabelSize::Small)
                .color(Color::Muted)
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_splits_into_paragraphs_bold_spans_and_a_diff() {
        let body = "First **bold** line\ncontinues.\n\nSecond.\n\n```diff\n+added\n-removed\n kept\n```\nAfter.\n";
        let blocks = parse_body(body);
        assert_eq!(
            blocks,
            vec![
                Block::Paragraph(Paragraph {
                    text: "First bold line continues.".into(),
                    bold: vec![6..10],
                }),
                Block::Paragraph(Paragraph {
                    text: "Second.".into(),
                    bold: Vec::new(),
                }),
                Block::Fence {
                    lang: "diff".into(),
                    lines: vec![
                        (LineKind::Added, "+added".into()),
                        (LineKind::Removed, "-removed".into()),
                        (LineKind::Context, " kept".into()),
                    ],
                },
                Block::Paragraph(Paragraph {
                    text: "After.".into(),
                    bold: Vec::new(),
                }),
            ]
        );
        // An unclosed fence runs to the end; a lone `**` stays text.
        assert_eq!(parse_body("a ** b").len(), 1);
        assert!(matches!(&parse_body("```\nx")[..], [Block::Fence { .. }]));
    }

    #[test]
    fn action_caps_name_a_key_and_a_label() {
        let cap = |name| {
            let cap = action_cap(name);
            (cap.key, cap.label)
        };
        // Only keys that are bound to the action get a cap; `a` is set on the primary action.
        assert_eq!(cap("accept"), ("".into(), "Accept".into()));
        assert_eq!(cap("open-run"), ("".into(), "Open run".into()));
        assert_eq!(cap("dismiss"), ("d".into(), "Dismiss".into()));
        assert_eq!(cap("open"), ("f".into(), "Open file".into()));
        assert_eq!(cap("some-thing"), ("".into(), "Some thing".into()));
    }
}
