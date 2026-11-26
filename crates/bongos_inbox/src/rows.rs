//! What the inbox list shows for each row, without rendering: the unread dot, the summary, the
//! status pill, the age and the `project · type · source` line. The inbox view renders these, and
//! `inbox: dump view` writes them as text.

use crate::Snapshot;
use bongos_inbox_model::format::Item;
use bongos_inbox_model::view_model::{self, Filter, GroupRow, ItemRow, Row};
use chrono::{DateTime, Utc};
use gpui::{App, Hsla};
use std::collections::HashMap;
use ui::prelude::ActiveTheme as _;

/// The opacity of a pill's fill: its status color at 16%.
pub const PILL_FILL_OPACITY: f32 = 0.16;

/// The header key an item sets to `unverified` when the approval it reports could not be checked.
pub const APPROVAL_KEY: &str = "approval";

/// A row's status pill. The item format has no field for it: [`pill_for`] derives it from the
/// item's kind and state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pill {
    Healed,
    NeedsYou,
    Blocked,
    Waiting,
    Stale,
    Unverified,
}

impl Pill {
    pub fn label(self) -> &'static str {
        match self {
            Pill::Healed => "healed",
            Pill::NeedsYou => "needs you",
            Pill::Blocked => "blocked",
            Pill::Waiting => "waiting",
            Pill::Stale => "stale",
            Pill::Unverified => "unverified",
        }
    }

    /// The pill's text color from the theme. The fill is this color at [`PILL_FILL_OPACITY`],
    /// except for [`Pill::Unverified`], which has a dashed outline and no fill.
    pub fn color(self, cx: &App) -> Hsla {
        let status = cx.theme().status();
        match self {
            Pill::Healed => status.success,
            Pill::NeedsYou => status.warning,
            Pill::Blocked | Pill::Unverified => status.error,
            Pill::Waiting => status.info,
            Pill::Stale => cx.theme().colors().text_placeholder,
        }
    }

    /// Whether the pill is drawn as a dashed outline without fill, so it never reads as approved.
    pub fn dashed(self) -> bool {
        self == Pill::Unverified
    }
}

/// The pill of an item of `kind` (a stale target wins over the kind). `extra` is the item's
/// unknown header keys: a `decision-default` item with `approval: unverified` is unverified.
pub fn pill_for(kind: &str, stale: bool, extra: &[(String, String)]) -> Option<Pill> {
    if stale {
        return Some(Pill::Stale);
    }
    match kind {
        "workflow-healed" => Some(Pill::Healed),
        "manual-check" => Some(Pill::NeedsYou),
        "rebase-needed" => Some(Pill::Blocked),
        "run-waiting" => Some(Pill::Waiting),
        "diagram-binding-stale" => Some(Pill::Stale),
        "decision-default"
            if extra
                .iter()
                .any(|(key, value)| key == APPROVAL_KEY && value == "unverified") =>
        {
            Some(Pill::Unverified)
        }
        _ => None,
    }
}

/// How a kind reads on a row: the known kinds in words, any other with its hyphens as spaces.
pub fn kind_label(kind: &str) -> String {
    let known = match kind {
        "decision-default" => "decision by default",
        "workflow-healed" => "workflow healed",
        "manual-check" => "manual check owed",
        "run-finished" => "run finished",
        "run-failed" => "run failed",
        "run-waiting" => "run waiting",
        "diagram-binding-stale" => "diagram binding",
        "rebase-needed" => "rebase needed",
        "pr-closed-outside" => "PR merged elsewhere",
        other => return other.replace('-', " "),
    };
    known.to_string()
}

/// The project and the rest of a source `<project>/<rest>`. A source without `/` is all
/// project.
pub fn split_source(source: &str) -> (&str, &str) {
    source.split_once('/').unwrap_or((source, ""))
}

/// Names a row across rescans: an item by its unique, a group by its key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Item(String),
    Group(String),
}

/// One row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewRow {
    pub key: RowKey,
    /// The unique and inode of an item row, which operations name it by; `None` for a group.
    pub item: Option<(String, u64)>,
    pub unread: bool,
    pub summary: String,
    pub pill: Option<Pill>,
    pub created: DateTime<Utc>,
    /// The second line's parts; empty parts are left out.
    pub project: String,
    pub kind: String,
    pub source: String,
}

impl ViewRow {
    /// The second line as text: `project · type · source`.
    pub fn second_line(&self) -> String {
        [&self.project, &self.kind, &self.source]
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn age(&self, now: DateTime<Utc>) -> String {
        view_model::age_text(self.created, now)
    }

    /// The row as one line of text: `>` on the cursor row, `*` when unread, then the summary, the
    /// pill in brackets, the age and the second line.
    pub fn text(&self, cursor: bool, now: DateTime<Utc>) -> String {
        let pill = self
            .pill
            .map(|pill| format!(" [{}]", pill.label()))
            .unwrap_or_default();
        format!(
            "{}{} {}{} {} | {}",
            if cursor { ">" } else { " " },
            if self.unread { "*" } else { " " },
            self.summary,
            pill,
            self.age(now),
            self.second_line()
        )
    }
}

/// The rows of `snapshot`, newest first, a group at the position of its newest member.
pub fn view_rows(snapshot: &Snapshot) -> Vec<ViewRow> {
    let items: HashMap<(&str, u64), &Item> = snapshot
        .items()
        .iter()
        .filter_map(|(entry, result)| {
            let item = result.as_ref().as_ref().ok()?;
            Some(((entry.unique.as_str(), entry.inode), item))
        })
        .collect();
    snapshot
        .rows(&Filter::All, &|_| false)
        .into_iter()
        .map(|row| match row {
            Row::Item(row) => {
                let extra = items
                    .get(&(row.unique.as_str(), row.inode))
                    .map_or(&[][..], |item| item.extra.as_slice());
                item_row(row, extra)
            }
            Row::Group(group) => group_row(group),
        })
        .collect()
}

fn item_row(row: ItemRow, extra: &[(String, String)]) -> ViewRow {
    let (project, source) = split_source(&row.source);
    ViewRow {
        key: RowKey::Item(row.unique.clone()),
        pill: pill_for(&row.kind, row.stale, extra),
        unread: !row.read,
        summary: row.summary,
        created: row.created,
        project: project.to_string(),
        kind: kind_label(&row.kind),
        source: source.to_string(),
        item: Some((row.unique, row.inode)),
    }
}

/// A group row has the same anatomy: the newest summary, then `<source>: <n> items (<u> unread)`.
fn group_row(group: GroupRow) -> ViewRow {
    ViewRow {
        key: RowKey::Group(group.key.clone()),
        item: None,
        unread: group.unread > 0,
        pill: None,
        created: group.created,
        project: String::new(),
        kind: group.label(),
        source: String::new(),
        summary: group.summary,
    }
}

/// The text `inbox: dump view` writes: `unread: <n>` (`off` while the inbox is off), then one
/// line per row.
pub fn dump_text(
    unread: Option<usize>,
    rows: &[ViewRow],
    cursor: Option<usize>,
    now: DateTime<Utc>,
) -> String {
    let mut text = match unread {
        Some(count) => format!("unread: {count}\n"),
        None => "unread: off\n".to_string(),
    };
    for (index, row) in rows.iter().enumerate() {
        text.push_str(&row.text(cursor == Some(index), now));
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn approval(value: &str) -> Vec<(String, String)> {
        vec![(APPROVAL_KEY.to_string(), value.to_string())]
    }

    #[test]
    fn the_pill_comes_from_the_kind_and_a_stale_target_wins() {
        let none: &[(String, String)] = &[];
        assert_eq!(pill_for("workflow-healed", false, none), Some(Pill::Healed));
        assert_eq!(pill_for("manual-check", false, none), Some(Pill::NeedsYou));
        assert_eq!(pill_for("rebase-needed", false, none), Some(Pill::Blocked));
        assert_eq!(pill_for("run-waiting", false, none), Some(Pill::Waiting));
        assert_eq!(
            pill_for("diagram-binding-stale", false, none),
            Some(Pill::Stale)
        );
        assert_eq!(
            pill_for("decision-default", false, &approval("unverified")),
            Some(Pill::Unverified)
        );
        assert_eq!(pill_for("decision-default", false, none), None);
        assert_eq!(
            pill_for("decision-default", false, &approval("verified")),
            None
        );
        for kind in [
            "run-finished",
            "run-failed",
            "pr-closed-outside",
            "note",
            "unreadable",
        ] {
            assert_eq!(pill_for(kind, false, none), None, "{kind}");
        }
        for kind in ["run-failed", "workflow-healed", "manual-check", "note"] {
            assert_eq!(pill_for(kind, true, none), Some(Pill::Stale), "{kind}");
        }
        assert_eq!(
            pill_for("decision-default", true, &approval("unverified")),
            Some(Pill::Stale)
        );
    }

    #[test]
    fn only_the_unverified_pill_is_dashed() {
        for pill in [
            Pill::Healed,
            Pill::NeedsYou,
            Pill::Blocked,
            Pill::Waiting,
            Pill::Stale,
        ] {
            assert!(!pill.dashed(), "{pill:?}");
        }
        assert!(Pill::Unverified.dashed());
        assert_eq!(Pill::NeedsYou.label(), "needs you");
    }

    #[test]
    fn the_age_counts_from_an_injected_now() {
        let now = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let row = |ago: Duration| ViewRow {
            key: RowKey::Item("x".into()),
            item: None,
            unread: false,
            summary: "x".into(),
            pill: None,
            created: now - ago,
            project: String::new(),
            kind: String::new(),
            source: String::new(),
        };
        assert_eq!(row(Duration::seconds(5)).age(now), "now");
        assert_eq!(row(Duration::minutes(3)).age(now), "3m");
        assert_eq!(row(Duration::minutes(59)).age(now), "59m");
        assert_eq!(row(Duration::hours(2)).age(now), "2h");
        assert_eq!(row(Duration::days(4)).age(now), "4d");
        // A clock behind the item's time reads as now.
        assert_eq!(row(Duration::minutes(-5)).age(now), "now");
    }

    #[test]
    fn the_second_line_is_project_type_and_source() {
        assert_eq!(
            split_source("bongos/overnight-refactor"),
            ("bongos", "overnight-refactor")
        );
        assert_eq!(split_source("a/b/c"), ("a", "b/c"));
        assert_eq!(split_source("solo"), ("solo", ""));
        assert_eq!(kind_label("decision-default"), "decision by default");
        assert_eq!(kind_label("some-new-kind"), "some new kind");
        let row = ViewRow {
            key: RowKey::Item("x".into()),
            item: None,
            unread: true,
            summary: "Chose a cache".into(),
            pill: Some(Pill::Unverified),
            created: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            project: "bongos".into(),
            kind: "decision by default".into(),
            source: "overnight-refactor".into(),
        };
        assert_eq!(
            row.second_line(),
            "bongos · decision by default · overnight-refactor"
        );
        let now = row.created + Duration::hours(1);
        assert_eq!(
            row.text(true, now),
            ">* Chose a cache [unverified] 1h | bongos · decision by default · overnight-refactor"
        );
        assert_eq!(
            dump_text(None, &[], None, now),
            "unread: off\n",
            "off says so"
        );
    }
}
