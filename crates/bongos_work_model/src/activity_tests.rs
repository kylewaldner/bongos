use std::time::SystemTime;

use chrono::{DateTime, TimeDelta, Utc};

use crate::activity::*;
use crate::fixtures::*;
use crate::store::WorkTree;
use crate::*;

fn utc(text: &str) -> DateTime<Utc> {
    to_utc(&at(text)).unwrap()
}

fn history(file: &mut TaskFile, when: &str) {
    file.history.push(HistoryEntry {
        at: at(when),
        by: "kyle".to_string(),
        event: "edited".to_string(),
        detail: None,
        extra: Extra::new(),
    });
}

fn live_project(slug: &str, created: &str) -> store::Project {
    project(
        slug,
        vec![with_created(leaf("work", Status::Open), created)],
    )
}

#[test]
fn activity_time_is_the_latest_of_created_history_and_mtime() {
    let mut t = with_created(leaf("a", Status::Open), "2026-11-01T00:00:00Z");
    assert_eq!(activity_time(&t), Some(utc("2026-11-01T00:00:00Z")));
    history(file_mut(&mut t), "2026-11-03T00:00:00Z");
    history(file_mut(&mut t), "2026-11-02T00:00:00Z");
    assert_eq!(activity_time(&t), Some(utc("2026-11-03T00:00:00Z")));
    t.files.insert(
        "chats/one.md".into(),
        SystemTime::from(utc("2026-11-05T12:00:00Z")),
    );
    assert_eq!(activity_time(&t), Some(utc("2026-11-05T12:00:00Z")));
    // An offset is honored.
    assert_eq!(
        utc("2026-11-01T02:00:00+02:00"),
        utc("2026-11-01T00:00:00Z")
    );
}

#[test]
fn a_group_takes_its_freshest_open_descendant() {
    let fresh_but_done = {
        let mut done = with_created(leaf("done", Status::Done), "2026-11-07T00:00:00Z");
        file_mut(&mut done).closed = Some(at("2026-11-07T00:00:00Z"));
        done
    };
    let t = tree(vec![group(
        "g",
        vec![
            with_created(leaf("old", Status::Open), "2026-11-01T00:00:00Z"),
            with_created(leaf("newer", Status::Open), "2026-11-02T00:00:00Z"),
            fresh_but_done,
        ],
    )]);
    assert_eq!(
        tree_activity_time(find(&t, "g"), &t),
        Some(utc("2026-11-02T00:00:00Z"))
    );
}

#[test]
fn fades_at_a_day_and_a_half_and_goes_stale_at_three() {
    let start = utc("2026-11-01T00:00:00Z");
    let t = WorkTree {
        projects: vec![live_project("p", "2026-11-01T00:00:00Z")],
        ..WorkTree::default()
    };
    let at_age = |age: TimeDelta| {
        freshness(
            Item::Project(&t.projects[0]),
            &t,
            start + age,
            FADE_AFTER,
            STALE_AFTER,
        )
    };
    assert_eq!(FADE_AFTER, TimeDelta::hours(36));
    assert_eq!(STALE_AFTER, TimeDelta::days(3));
    assert_eq!(at_age(TimeDelta::minutes(35 * 60 + 59)), Freshness::Live);
    assert_eq!(at_age(TimeDelta::hours(36)), Freshness::Fading);
    assert_eq!(at_age(TimeDelta::hours(71)), Freshness::Fading);
    assert_eq!(at_age(TimeDelta::hours(72)), Freshness::Stale);

    let mut archived = live_project("q", "2026-11-01T00:00:00Z");
    archived.file.as_mut().unwrap().archived = true;
    let t = WorkTree {
        projects: vec![archived],
        ..WorkTree::default()
    };
    assert_eq!(
        freshness(
            Item::Project(&t.projects[0]),
            &t,
            start,
            FADE_AFTER,
            STALE_AFTER
        ),
        Freshness::Closed
    );
}

fn slugs<'a>(entries: &'a [Entry<'a>]) -> Vec<&'a str> {
    entries.iter().map(|entry| entry.item.slug()).collect()
}

#[test]
fn top_level_sorts_open_stale_and_closed() {
    let now = utc("2026-11-08T00:00:00Z");
    let mut archived = live_project("archived", "2026-11-01T00:00:00Z");
    archived.file.as_mut().unwrap().archived = true;
    file_mut(&mut archived.task).closed = Some(at("2026-11-03T00:00:00Z"));
    let mut finished_oneoff = with_created(
        leaf("2026-11-04-done", Status::Done),
        "2026-11-04T00:00:00Z",
    );
    file_mut(&mut finished_oneoff).closed = Some(at("2026-11-05T00:00:00Z"));
    let t = WorkTree {
        projects: vec![
            live_project("older", "2026-11-06T06:00:00Z"),
            live_project("newer", "2026-11-07T22:00:00Z"),
            live_project("forgotten", "2026-11-02T00:00:00Z"),
            archived,
        ],
        activities: vec![activity(
            "triage",
            vec![
                with_created(
                    leaf("2026-11-07-fresh", Status::Open),
                    "2026-11-07T23:00:00Z",
                ),
                finished_oneoff,
            ],
        )],
        ..WorkTree::default()
    };
    let top = top_level(&t, now, FADE_AFTER, STALE_AFTER);
    assert_eq!(slugs(&top.open), ["triage", "newer", "older"]);
    assert_eq!(top.open[2].freshness, Freshness::Fading);
    assert_eq!(slugs(&top.stale), ["forgotten"]);
    assert_eq!(slugs(&top.closed), ["2026-11-04-done", "archived"]);
}
