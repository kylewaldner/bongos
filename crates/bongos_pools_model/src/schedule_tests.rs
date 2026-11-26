use std::time::Duration;

use crate::schedule::{
    Schedule, TimeOfDay, Weekday, parse_duration, parse_every, parse_time, parse_timer,
    parse_weekday,
};

fn timer(text: &str) -> Result<Schedule, String> {
    parse_timer(&toml::from_str::<toml::Table>(text).unwrap())
}

#[test]
fn times_are_hh_mm() {
    assert_eq!(parse_time("03:00"), Ok(TimeOfDay { hour: 3, minute: 0 }));
    assert_eq!(
        parse_time("23:59"),
        Ok(TimeOfDay {
            hour: 23,
            minute: 59
        })
    );
    for bad in [
        "3:00", "24:00", "12:60", "12-00", "1200", "ab:cd", "", "12:5", "+1:00",
    ] {
        assert!(parse_time(bad).is_err(), "{bad}");
    }
}

#[test]
fn weekdays_are_names() {
    assert_eq!(parse_weekday("mon"), Ok(Weekday::Mon));
    assert_eq!(parse_weekday("Sunday"), Ok(Weekday::Sun));
    assert_eq!(parse_weekday("FRI"), Ok(Weekday::Fri));
    assert!(parse_weekday("monday2").is_err());
    assert!(parse_weekday("1").is_err());
}

#[test]
fn every_is_at_least_a_minute() {
    assert_eq!(parse_every("1m"), Ok(Duration::from_secs(60)));
    assert_eq!(parse_every("15m"), Ok(Duration::from_secs(900)));
    assert_eq!(parse_every("2h"), Ok(Duration::from_secs(7200)));
    assert_eq!(parse_every("1d"), Ok(Duration::from_secs(86_400)));
    for bad in ["30s", "0m", "m", "5", "5x", "-5m", "1.5h", ""] {
        assert!(parse_every(bad).is_err(), "{bad}");
    }
}

#[test]
fn durations_allow_seconds() {
    assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
    assert_eq!(parse_duration("4h"), Ok(Duration::from_secs(14_400)));
    assert!(parse_duration("0h").is_err());
    assert!(parse_duration("h").is_err());
    assert!(parse_duration("99999999999999999999d").is_err());
}

#[test]
fn timers_parse() {
    assert_eq!(
        timer("at = [\"03:00\", \"15:30\"]\ndays = [\"mon\", \"fri\"]"),
        Ok(Schedule::At {
            times: vec![
                TimeOfDay { hour: 3, minute: 0 },
                TimeOfDay {
                    hour: 15,
                    minute: 30
                }
            ],
            days: Some(vec![Weekday::Mon, Weekday::Fri])
        })
    );
    assert_eq!(
        timer("at = [\"03:00\"]"),
        Ok(Schedule::At {
            times: vec![TimeOfDay { hour: 3, minute: 0 }],
            days: None
        })
    );
    assert_eq!(
        timer("every = \"90m\""),
        Ok(Schedule::Every(Duration::from_secs(5400)))
    );
}

#[test]
fn bad_timers_are_refused() {
    for bad in [
        "at = [\"3am\"]",
        "at = []",
        "at = [\"03:00\"]\ndays = [\"funday\"]",
        "at = [\"03:00\"]\ndays = []",
        "at = [\"03:00\"]\nevery = \"5m\"",
        "every = \"30s\"",
        "every = \"5m\"\ndays = [\"mon\"]",
        "days = [\"mon\"]",
        "",
        "when = \"noon\"",
        "at = \"03:00\"",
    ] {
        assert!(timer(bad).is_err(), "{bad}");
    }
}
