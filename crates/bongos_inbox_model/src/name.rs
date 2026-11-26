//! Maildir-style item file names: `<unique>` or `<unique>:2,<flags>`.

use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

pub const SEEN: char = 'S';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError(pub String);

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NameError {}

/// Flag letters, kept sorted and deduplicated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Flags(BTreeSet<char>);

impl Flags {
    pub fn contains(&self, flag: char) -> bool {
        self.0.contains(&flag)
    }

    pub fn as_string(&self) -> String {
        self.0.iter().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub unique: String,
    /// `None` for a name without a suffix (delivered in `new/`).
    pub flags: Option<Flags>,
}

impl ParsedName {
    pub fn is_read(&self) -> bool {
        self.flags.as_ref().is_some_and(|flags| flags.contains(SEEN))
    }

    pub fn render(&self) -> String {
        match &self.flags {
            None => self.unique.clone(),
            Some(flags) => render(&self.unique, flags),
        }
    }

    pub fn with_flag_added(&self, flag: char) -> Self {
        let mut flags = self.flags.clone().unwrap_or_default();
        flags.0.insert(flag);
        Self {
            unique: self.unique.clone(),
            flags: Some(flags),
        }
    }

    pub fn with_flag_removed(&self, flag: char) -> Self {
        let mut flags = self.flags.clone().unwrap_or_default();
        flags.0.remove(&flag);
        Self {
            unique: self.unique.clone(),
            flags: Some(flags),
        }
    }
}

fn check_unique(unique: &str) -> Result<(), NameError> {
    if unique.is_empty() {
        return Err(NameError("empty name".into()));
    }
    if unique.starts_with('.') {
        return Err(NameError("name starts with a dot".into()));
    }
    Ok(())
}

pub fn parse_name(file_name: &str) -> Result<ParsedName, NameError> {
    let Some((unique, suffix)) = file_name.split_once(':') else {
        check_unique(file_name)?;
        return Ok(ParsedName {
            unique: file_name.to_string(),
            flags: None,
        });
    };
    check_unique(unique)?;
    let Some(letters) = suffix.strip_prefix("2,") else {
        return Err(NameError(format!("malformed suffix: :{suffix}")));
    };
    if !letters.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(NameError(format!("malformed flags: {letters}")));
    }
    Ok(ParsedName {
        unique: unique.to_string(),
        flags: Some(Flags(letters.chars().collect())),
    })
}

pub fn render(unique: &str, flags: &Flags) -> String {
    format!("{unique}:2,{}", flags.as_string())
}

/// Lowercase `[a-z0-9-]` slug; runs of other characters become one `-`.
pub fn slug(source: &str) -> String {
    let mut out = String::new();
    for c in source.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() { "item".into() } else { out.into() }
}

/// `<YYYYMMDDTHHMMSSZ>-<source-slug>-<hex8>.md`
pub fn unique_name(created: DateTime<Utc>, source: &str, hex8: &str) -> String {
    format!(
        "{}-{}-{}.md",
        created.format("%Y%m%dT%H%M%SZ"),
        slug(source),
        hex8
    )
}

/// Like [`unique_name`], with the hex taken from a fresh random UUID.
pub fn unique_name_random(created: DateTime<Utc>, source: &str) -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    unique_name(created, source, &hex[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn no_suffix_has_no_flags() {
        let parsed = parse_name("a-b.md").unwrap();
        assert_eq!(parsed.flags, None);
        assert!(!parsed.is_read());
        assert_eq!(parsed.render(), "a-b.md");
    }

    #[test]
    fn flags_sorted_and_keep_f_and_t() {
        let parsed = parse_name("a.md:2,TSF").unwrap();
        assert_eq!(parsed.flags.as_ref().unwrap().as_string(), "FST");
        assert_eq!(parsed.render(), "a.md:2,FST");
        assert!(parsed.is_read());
        assert_eq!(parse_name("a.md:2,").unwrap().render(), "a.md:2,");
        assert_eq!(parse_name("a.md:2,SS").unwrap().render(), "a.md:2,S");
    }

    #[test]
    fn add_and_remove_seen_keeps_others() {
        let parsed = parse_name("a.md:2,TF").unwrap();
        let added = parsed.with_flag_added('S');
        assert_eq!(added.render(), "a.md:2,FST");
        assert_eq!(added.with_flag_removed('S').render(), "a.md:2,FT");
        assert_eq!(
            parse_name("a.md").unwrap().with_flag_added('S').render(),
            "a.md:2,S"
        );
    }

    #[test]
    fn bad_names_rejected() {
        for bad in [
            ".hidden.md",
            "",
            ":2,S",
            "a.md:1,S",
            "a.md:2",
            "a.md:2,S1",
            "a.md:2,S:x",
            "a.md:",
        ] {
            assert!(parse_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn unique_name_form() {
        let created = Utc.with_ymd_and_hms(2026, 11, 11, 3, 12, 45).unwrap();
        assert_eq!(
            unique_name(created, "agent/Plan Review", "0a1b2c3d"),
            "20261111T031245Z-agent-plan-review-0a1b2c3d.md"
        );
        assert_eq!(slug("///"), "item");
    }
}
