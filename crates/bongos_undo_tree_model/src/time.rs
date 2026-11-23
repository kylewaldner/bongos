//! Relative times for undo states: "just now", "3 min ago", "yesterday", never a calendar date.

use std::time::Instant;

/// A source of the current time, so a test can hand in a fixed one.
pub trait Now {
    /// The current instant.
    fn now(&self) -> Instant;
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How long ago `then` was, as `now` sees it: "just now" under a minute (and when `then` is after
/// `now`), then "N min ago", "N h ago" under 24 hours, "yesterday" from 24 to under 48 hours and
/// "N days ago" from 48 hours on.
pub fn relative_time(then: Instant, now: Instant) -> String {
    let elapsed = now.saturating_duration_since(then);
    let seconds = elapsed.as_secs();
    if seconds < MINUTE {
        "just now".to_string()
    } else if seconds < HOUR {
        format!("{} min ago", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{} h ago", seconds / HOUR)
    } else if seconds < 2 * DAY {
        "yesterday".to_string()
    } else {
        format!("{} days ago", seconds / DAY)
    }
}
