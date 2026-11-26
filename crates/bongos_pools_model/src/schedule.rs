//! Schedules of timer triggers: `at` times of day with optional weekdays, or an `every` interval.
//! Pure parsing of the text; nothing here reads a clock.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// The listed local times, on the listed days (every day when `days` is `None`).
    At {
        times: Vec<TimeOfDay>,
        days: Option<Vec<Weekday>>,
    },
    /// An interval of at least one minute.
    Every(Duration),
}

/// `HH:MM`, two digits each, 00:00 to 23:59.
pub fn parse_time(text: &str) -> Result<TimeOfDay, String> {
    let bad = || format!("\"{text}\" is not a time of day written HH:MM");
    let (hour, minute) = text.split_once(':').ok_or_else(bad)?;
    let two_digits = |part: &str| part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(hour) || !two_digits(minute) {
        return Err(bad());
    }
    let hour: u8 = hour.parse().map_err(|_| bad())?;
    let minute: u8 = minute.parse().map_err(|_| bad())?;
    if hour > 23 || minute > 59 {
        return Err(bad());
    }
    Ok(TimeOfDay { hour, minute })
}

/// A weekday name: `mon` to `sun` or the full English name, in any case.
pub fn parse_weekday(text: &str) -> Result<Weekday, String> {
    let day = match text.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return Err(format!("\"{text}\" is not a weekday name (mon to sun)")),
    };
    Ok(day)
}

/// `<n>s|m|h|d` with `n >= 1`, as `max_hold` and hook timeouts are written.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("\"{text}\" is not a duration like 30s, 10m, 4h or 2d");
    let unit = text.chars().last().ok_or_else(bad)?;
    let seconds_per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = digits.parse().map_err(|_| bad())?;
    if count == 0 {
        return Err(format!("\"{text}\" must be at least one"));
    }
    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(bad)
}

/// `<n>m|h|d` with a result of at least one minute (seconds are refused).
pub fn parse_every(text: &str) -> Result<Duration, String> {
    if text.ends_with('s') {
        return Err(format!("every \"{text}\" is below one minute"));
    }
    parse_duration(text)
}

/// The value of a `timer` condition: `{ at = [..], days = [..] }` or `{ every = ".." }`.
pub fn parse_timer(table: &toml::Table) -> Result<Schedule, String> {
    for key in table.keys() {
        if !["at", "days", "every"].contains(&key.as_str()) {
            return Err(format!("unknown key \"{key}\" in timer"));
        }
    }
    match (table.get("at"), table.get("every")) {
        (Some(_), Some(_)) => Err("a timer has `at` or `every`, not both".to_string()),
        (None, None) => Err("a timer needs `at` or `every`".to_string()),
        (None, Some(every)) => {
            if table.contains_key("days") {
                return Err("`days` goes with `at`, not `every`".to_string());
            }
            let text = every
                .as_str()
                .ok_or("`every` must be a string like \"15m\"")?;
            parse_every(text).map(Schedule::Every)
        }
        (Some(at), None) => {
            let at = at.as_array().ok_or("`at` must be a list of HH:MM times")?;
            if at.is_empty() {
                return Err("`at` needs at least one time".to_string());
            }
            let times = at
                .iter()
                .map(|time| {
                    time.as_str()
                        .ok_or_else(|| "`at` holds HH:MM strings".to_string())
                        .and_then(parse_time)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let days = match table.get("days") {
                None => None,
                Some(days) => {
                    let days = days.as_array().ok_or("`days` must be a list of weekdays")?;
                    if days.is_empty() {
                        return Err("`days` needs at least one weekday".to_string());
                    }
                    Some(
                        days.iter()
                            .map(|day| {
                                day.as_str()
                                    .ok_or_else(|| "`days` holds weekday names".to_string())
                                    .and_then(parse_weekday)
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                }
            };
            Ok(Schedule::At { times, days })
        }
    }
}
