use chrono::{NaiveDate, NaiveDateTime};

pub const DEFAULT_OPEN: &str = "=== prompt";
pub const DEFAULT_CLOSE: &str = "===";

const SENT_INFIX: &str = " | sent ";
const STAMP_LEN: usize = 16;

/// What one marker line says: when the block was created and, once sent, when it last was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerLine {
    pub created: NaiveDateTime,
    pub sent: Option<NaiveDateTime>,
}

impl MarkerLine {
    pub fn pending(created: NaiveDateTime) -> Self {
        Self {
            created,
            sent: None,
        }
    }

    pub fn is_sent(&self) -> bool {
        self.sent.is_some()
    }
}

/// The configured open and close strings. The default syntax is always recognized as well, so
/// changing the setting never orphans old files; new markers are written in the configured one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerSyntax {
    open: String,
    close: String,
}

impl Default for MarkerSyntax {
    fn default() -> Self {
        Self::new(DEFAULT_OPEN, DEFAULT_CLOSE)
    }
}

impl MarkerSyntax {
    /// An empty open string falls back to the default open and an empty close to the default close.
    pub fn new(open: &str, close: &str) -> Self {
        let open = open.trim();
        let close = close.trim();
        Self {
            open: if open.is_empty() { DEFAULT_OPEN } else { open }.to_string(),
            close: if close.is_empty() {
                DEFAULT_CLOSE
            } else {
                close
            }
            .to_string(),
        }
    }

    pub fn open(&self) -> &str {
        &self.open
    }

    pub fn close(&self) -> &str {
        &self.close
    }

    /// Parses one line (no line break) as a marker in the configured or the default syntax.
    /// Anything that is not exactly a marker is body text.
    pub fn parse(&self, line: &str) -> Option<MarkerLine> {
        parse_with(line, &self.open, &self.close).or_else(|| {
            if self.open == DEFAULT_OPEN && self.close == DEFAULT_CLOSE {
                None
            } else {
                parse_with(line, DEFAULT_OPEN, DEFAULT_CLOSE)
            }
        })
    }

    /// The line to write for a marker, in the configured syntax, without a line break.
    pub fn render(&self, marker: &MarkerLine) -> String {
        let mut line = format!("{} {}", self.open, format_stamp(&marker.created));
        if let Some(sent) = &marker.sent {
            line.push_str(SENT_INFIX);
            line.push_str(&format_stamp(sent));
        }
        line.push(' ');
        line.push_str(&self.close);
        line
    }
}

/// `YYYY-MM-DD HH:MM`, the form both stamps take on disk (seconds are dropped).
pub fn format_stamp(stamp: &NaiveDateTime) -> String {
    stamp.format("%Y-%m-%d %H:%M").to_string()
}

fn parse_with(line: &str, open: &str, close: &str) -> Option<MarkerLine> {
    let line = line.trim_end();
    let rest = line.strip_prefix(open)?.strip_prefix(' ')?;
    let rest = rest.strip_suffix(close)?.strip_suffix(' ')?;
    if rest.len() == STAMP_LEN {
        return Some(MarkerLine::pending(parse_stamp(rest)?));
    }
    let (created, sent) = rest.split_once(SENT_INFIX)?;
    Some(MarkerLine {
        created: parse_stamp(created)?,
        sent: Some(parse_stamp(sent)?),
    })
}

fn parse_stamp(text: &str) -> Option<NaiveDateTime> {
    let bytes = text.as_bytes();
    if bytes.len() != STAMP_LEN {
        return None;
    }
    for (i, byte) in bytes.iter().enumerate() {
        let ok = match i {
            4 | 7 => *byte == b'-',
            10 => *byte == b' ',
            13 => *byte == b':',
            _ => byte.is_ascii_digit(),
        };
        if !ok {
            return None;
        }
    }
    let number = |range: std::ops::Range<usize>| text[range].parse::<u32>().ok();
    NaiveDate::from_ymd_opt(number(0..4)? as i32, number(5..7)?, number(8..10)?)?.and_hms_opt(
        number(11..13)?,
        number(14..16)?,
        0,
    )
}
