//! Tests of `relative_time`, on fixed offsets from one base instant.

use std::time::{Duration, Instant};

use crate::relative_time;

fn ago(base: Instant, seconds: u64) -> String {
    relative_time(base, base + Duration::from_secs(seconds))
}

#[test]
fn undo_tree_under_a_minute_is_just_now() {
    let base = Instant::now();
    assert_eq!(ago(base, 0), "just now");
    assert_eq!(ago(base, 59), "just now");
    // A state from the future is treated as just now.
    assert_eq!(
        relative_time(base + Duration::from_secs(30), base),
        "just now"
    );
}

#[test]
fn undo_tree_minutes_and_hours_count_up() {
    let base = Instant::now();
    assert_eq!(ago(base, 3 * 60), "3 min ago");
    assert_eq!(ago(base, 5 * 60 * 60), "5 h ago");
}

#[test]
fn undo_tree_a_day_is_yesterday_and_two_days_count_days() {
    let base = Instant::now();
    assert_eq!(ago(base, 25 * 60 * 60), "yesterday");
    assert_eq!(ago(base, 3 * 24 * 60 * 60), "3 days ago");
}
