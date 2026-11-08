//! When things last moved and how fresh a top-level item is: a task's activity time, Live,
//! Fading and Stale, and the order of the top-level list. Pure functions; the mtimes come from
//! what the loader already collected in [`Task::files`].

use chrono::{DateTime, TimeDelta, Utc};
use toml::value::Offset;

use crate::derive::{self, Shape, State};
use crate::store::{Activity, Project, Task, WorkTree};
use crate::types::Timestamp;

/// An item fades after this long without activity.
pub const FADE_AFTER: TimeDelta = TimeDelta::hours(36);

/// An item is stale after this long without activity.
pub const STALE_AFTER: TimeDelta = TimeDelta::days(3);

/// A TOML datetime as a UTC instant. A datetime with no offset is read as UTC; one with no date
/// has no instant.
pub fn to_utc(timestamp: &Timestamp) -> Option<DateTime<Utc>> {
    let date = timestamp.date?;
    let day = chrono::NaiveDate::from_ymd_opt(
        i32::from(date.year),
        u32::from(date.month),
        u32::from(date.day),
    )?;
    let local = match timestamp.time {
        Some(time) => day.and_hms_nano_opt(
            u32::from(time.hour),
            u32::from(time.minute),
            u32::from(time.second),
            time.nanosecond,
        )?,
        None => day.and_hms_opt(0, 0, 0)?,
    };
    let minutes = match timestamp.offset {
        Some(Offset::Custom { minutes }) => i64::from(minutes),
        _ => 0,
    };
    Some(local.and_utc() - TimeDelta::minutes(minutes))
}

/// When the task was created; the earliest possible instant when unknown.
pub fn created_at(task: &Task) -> DateTime<Utc> {
    task.file
        .as_ref()
        .and_then(|file| to_utc(&file.created))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The latest of the task's `created`, the last `history.at` and the newest mtime of the files the
/// loader collected for it. `None` when it has none of the three.
pub fn activity_time(task: &Task) -> Option<DateTime<Utc>> {
    let file = task.file.as_ref();
    let created = file.and_then(|file| to_utc(&file.created));
    let history = file.and_then(|file| file.history.iter().filter_map(|e| to_utc(&e.at)).max());
    let mtime = task.newest_mtime().map(DateTime::<Utc>::from);
    [created, history, mtime].into_iter().flatten().max()
}

/// Whether the task is finished: Done or Cancelled.
pub fn is_closed(task: &Task, tree: &WorkTree) -> bool {
    matches!(derive::state(task, tree), State::Done | State::Cancelled)
}

/// A leaf's own activity time; a group's is its freshest open descendant's (its own when it has
/// none).
pub fn tree_activity_time(task: &Task, tree: &WorkTree) -> Option<DateTime<Utc>> {
    if derive::shape(task, tree) == Shape::Leaf {
        return activity_time(task);
    }
    task.walk()
        .into_iter()
        .skip(1)
        .filter(|descendant| !is_closed(descendant, tree))
        .filter_map(activity_time)
        .max()
        .or_else(|| activity_time(task))
}

/// A top-level row.
#[derive(Clone, Copy, Debug)]
pub enum Item<'a> {
    Project(&'a Project),
    /// An activity group of one-offs; its open one-offs are what count.
    Activity(&'a Activity),
    /// One one-off, listed on its own once closed.
    Task(&'a Task),
}

impl Item<'_> {
    pub fn slug(&self) -> &str {
        match self {
            Item::Project(project) => &project.slug,
            Item::Activity(activity) => &activity.slug,
            Item::Task(task) => &task.slug,
        }
    }

    fn is_closed(&self, tree: &WorkTree) -> bool {
        match self {
            Item::Project(project) => {
                project.file.as_ref().is_some_and(|file| file.archived)
                    || is_closed(&project.task, tree)
            }
            Item::Activity(activity) => activity
                .oneoffs
                .iter()
                .all(|oneoff| is_closed(oneoff, tree)),
            Item::Task(task) => is_closed(task, tree),
        }
    }

    /// The item's activity time. An activity group's is its freshest open one-off's.
    pub fn activity_time(&self, tree: &WorkTree) -> Option<DateTime<Utc>> {
        match self {
            Item::Project(project) => tree_activity_time(&project.task, tree),
            Item::Activity(activity) => activity
                .oneoffs
                .iter()
                .filter(|oneoff| !is_closed(oneoff, tree))
                .filter_map(|oneoff| tree_activity_time(oneoff, tree))
                .max(),
            Item::Task(task) => tree_activity_time(task, tree),
        }
    }

    /// When a closed item was closed: its `closed` time, else its last activity.
    fn closed_at(&self, tree: &WorkTree) -> Option<DateTime<Utc>> {
        let task = match self {
            Item::Project(project) => &project.task,
            Item::Task(task) => task,
            Item::Activity(_) => return self.activity_time(tree),
        };
        task.file
            .as_ref()
            .and_then(|file| file.closed.as_ref().and_then(to_utc))
            .or_else(|| activity_time(task))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Live,
    Fading,
    Stale,
    Closed,
}

/// How fresh `item` is at `now`: Closed when it is closed, else Live below `fade`, Fading from
/// `fade` up to `stale`, Stale from `stale`. An item with no known activity time is Live.
pub fn freshness(
    item: Item,
    tree: &WorkTree,
    now: DateTime<Utc>,
    fade: TimeDelta,
    stale: TimeDelta,
) -> Freshness {
    if item.is_closed(tree) {
        return Freshness::Closed;
    }
    let Some(time) = item.activity_time(tree) else {
        return Freshness::Live;
    };
    let age = now - time;
    if age >= stale {
        Freshness::Stale
    } else if age >= fade {
        Freshness::Fading
    } else {
        Freshness::Live
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub item: Item<'a>,
    pub freshness: Freshness,
}

/// The top-level list in its three parts.
#[derive(Clone, Debug, Default)]
pub struct TopLevel<'a> {
    /// Open projects and one-off activity groups, Live or Fading, newest activity first.
    pub open: Vec<Entry<'a>>,
    /// Open items gone Stale, newest activity first.
    pub stale: Vec<Entry<'a>>,
    /// Closed one-offs, and projects that are archived or finished, newest close date first.
    pub closed: Vec<Entry<'a>>,
}

/// Sorts the tree's top level. Ties break on slug so the order is the same every time.
pub fn top_level<'a>(
    tree: &'a WorkTree,
    now: DateTime<Utc>,
    fade: TimeDelta,
    stale: TimeDelta,
) -> TopLevel<'a> {
    let mut items: Vec<Item> = tree.projects.iter().map(Item::Project).collect();
    for activity in &tree.activities {
        if activity
            .oneoffs
            .iter()
            .any(|oneoff| !is_closed(oneoff, tree))
        {
            items.push(Item::Activity(activity));
        }
        items.extend(
            activity
                .oneoffs
                .iter()
                .filter(|oneoff| is_closed(oneoff, tree))
                .map(Item::Task),
        );
    }
    let mut top = TopLevel::default();
    for item in items {
        let freshness = freshness(item, tree, now, fade, stale);
        let entry = Entry { item, freshness };
        match freshness {
            Freshness::Closed => top.closed.push(entry),
            Freshness::Stale => top.stale.push(entry),
            Freshness::Live | Freshness::Fading => top.open.push(entry),
        }
    }
    let newest_first = |time: &dyn Fn(&Item) -> Option<DateTime<Utc>>, entries: &mut Vec<Entry>| {
        entries.sort_by(|a, b| {
            time(&b.item)
                .cmp(&time(&a.item))
                .then_with(|| a.item.slug().cmp(b.item.slug()))
        });
    };
    newest_first(&|item| item.activity_time(tree), &mut top.open);
    newest_first(&|item| item.activity_time(tree), &mut top.stale);
    newest_first(&|item| item.closed_at(tree), &mut top.closed);
    top
}
