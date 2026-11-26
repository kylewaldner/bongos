//! The inbox item file format: a `---` delimited header of `key: value`
//! lines followed by a free-form body.

use chrono::{DateTime, SecondsFormat, Utc};

/// Files larger than this load only their header.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// The header must end within this many bytes.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// The newest item format version this build can read.
pub const SUPPORTED_VERSION: u32 = 1;

const REQUIRED: [&str; 6] = [
    "inbox-version",
    "kind",
    "created",
    "source",
    "summary",
    "actions",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub version: u32,
    pub kind: String,
    pub created: DateTime<Utc>,
    pub source: String,
    pub group: Option<String>,
    pub target: Option<String>,
    pub summary: String,
    pub actions: Vec<String>,
    /// Unknown header keys, in file order.
    pub extra: Vec<(String, String)>,
    pub body: String,
    /// False when the file was over [`MAX_FILE_BYTES`] and only the header was read.
    pub body_loaded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unreadable {
    pub reason: String,
    /// 1-based line number where known.
    pub line: Option<usize>,
}

impl Unreadable {
    fn new(reason: impl Into<String>, line: Option<usize>) -> Self {
        Self {
            reason: reason.into(),
            line,
        }
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

impl std::error::Error for Unreadable {}

fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

pub fn parse(bytes: &[u8]) -> Result<Item, Unreadable> {
    let oversize = bytes.len() > MAX_FILE_BYTES;
    let mut pos = 0usize;
    let mut line_no = 0usize;
    let mut header: Vec<(usize, String)> = Vec::new();
    let mut closed = false;

    while pos < bytes.len() {
        line_no += 1;
        let end = bytes[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| pos + i);
        let line = strip_cr(&bytes[pos..end]);
        let next = (end + 1).min(bytes.len());
        if line_no == 1 {
            if line != b"---" {
                return Err(Unreadable::new("missing header start (---)", Some(1)));
            }
        } else if line == b"---" {
            closed = true;
            pos = next;
            break;
        } else {
            header.push((line_no, String::from_utf8_lossy(line).into_owned()));
        }
        pos = next;
        if pos > MAX_HEADER_BYTES {
            return Err(Unreadable::new(
                "header not terminated within 64 KiB",
                Some(line_no),
            ));
        }
    }
    if line_no == 0 {
        return Err(Unreadable::new("empty file", None));
    }
    if !closed {
        return Err(Unreadable::new("header not terminated (missing ---)", None));
    }
    if pos > MAX_HEADER_BYTES {
        return Err(Unreadable::new(
            "header not terminated within 64 KiB",
            Some(line_no),
        ));
    }

    let mut fields: Vec<(String, String)> = Vec::new();
    for (n, line) in &header {
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(Unreadable::new("header line without a colon", Some(*n)));
        };
        fields.push((key.trim().to_string(), value.trim().to_string()));
    }
    let line_of = |key: &str| {
        header
            .iter()
            .filter(|(_, l)| l.split_once(':').is_some_and(|(k, _)| k.trim() == key))
            .map(|(n, _)| *n)
            .next_back()
    };
    let get = |key: &str| {
        fields
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    let version_text = get("inbox-version");
    let version = match &version_text {
        Some(text) => {
            let version: u32 = text.parse().map_err(|_| {
                Unreadable::new("inbox-version is not an integer", line_of("inbox-version"))
            })?;
            if version > SUPPORTED_VERSION {
                return Err(Unreadable::new(
                    format!("newer item format (version {version})"),
                    line_of("inbox-version"),
                ));
            }
            version
        }
        None => 0,
    };
    for key in REQUIRED {
        if get(key).is_none() {
            return Err(Unreadable::new(
                format!("missing required field: {key}"),
                None,
            ));
        }
    }

    let created_text = get("created").unwrap_or_default();
    let created = DateTime::parse_from_rfc3339(&created_text)
        .map_err(|error| {
            Unreadable::new(
                format!("bad created value {created_text:?}: {error}"),
                line_of("created"),
            )
        })?
        .with_timezone(&Utc);

    let actions_text = get("actions").unwrap_or_default();
    let Some(inner) = actions_text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Err(Unreadable::new(
            "actions must be a list like [a, b]",
            line_of("actions"),
        ));
    };
    let actions = inner
        .split(',')
        .map(str::trim)
        .filter(|action| !action.is_empty())
        .map(str::to_string)
        .collect();

    const KNOWN: [&str; 8] = [
        "inbox-version",
        "kind",
        "created",
        "source",
        "summary",
        "actions",
        "group",
        "target",
    ];
    let extra = fields
        .iter()
        .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
        .cloned()
        .collect();

    let (body, body_loaded) = if oversize {
        (String::new(), false)
    } else {
        let rest = &bytes[pos.min(bytes.len())..];
        (String::from_utf8_lossy(rest).into_owned(), true)
    };
    // The closing `---` line's own terminator was consumed above, so the
    // body is exactly what follows it.
    Ok(Item {
        version,
        kind: get("kind").unwrap_or_default(),
        created,
        source: get("source").unwrap_or_default(),
        group: get("group"),
        target: get("target"),
        summary: get("summary").unwrap_or_default(),
        actions,
        extra,
        body,
        body_loaded,
    })
}

pub fn render(item: &Item) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("inbox-version: {}\n", item.version));
    out.push_str(&format!("kind: {}\n", item.kind));
    out.push_str(&format!(
        "created: {}\n",
        item.created.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    ));
    out.push_str(&format!("source: {}\n", item.source));
    if let Some(group) = &item.group {
        out.push_str(&format!("group: {group}\n"));
    }
    if let Some(target) = &item.target {
        out.push_str(&format!("target: {target}\n"));
    }
    out.push_str(&format!("summary: {}\n", item.summary));
    out.push_str(&format!("actions: [{}]\n", item.actions.join(", ")));
    for (key, value) in &item.extra {
        out.push_str(&format!("{key}: {value}\n"));
    }
    out.push_str("---\n");
    out.push_str(&item.body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRD: &str = "---\ninbox-version: 1\nkind: decision-default\ncreated: 2026-11-11T03:12:45Z\nsource: agent/plan-review\ngroup: agent/plan-review/run-7\ntarget: plan.md\nsummary: Took the default for \"tab width\"\nactions: [accept, overturn]\n---\nBody text.\n";

    fn minimal(without: &str) -> String {
        let mut text = String::from("---\n");
        for (k, v) in [
            ("inbox-version", "1"),
            ("kind", "k"),
            ("created", "2026-11-11T03:12:45Z"),
            ("source", "s"),
            ("summary", "sum"),
            ("actions", "[a]"),
        ] {
            if k != without {
                text.push_str(&format!("{k}: {v}\n"));
            }
        }
        text.push_str("---\n");
        text
    }

    #[test]
    fn prd_example_parses() {
        let item = parse(PRD.as_bytes()).unwrap();
        assert_eq!(item.version, 1);
        assert_eq!(item.kind, "decision-default");
        assert_eq!(item.created.to_rfc3339(), "2026-11-11T03:12:45+00:00");
        assert_eq!(item.source, "agent/plan-review");
        assert_eq!(item.group.as_deref(), Some("agent/plan-review/run-7"));
        assert_eq!(item.target.as_deref(), Some("plan.md"));
        assert_eq!(item.summary, "Took the default for \"tab width\"");
        assert_eq!(item.actions, vec!["accept", "overturn"]);
        assert_eq!(item.body, "Body text.\n");
        assert!(item.body_loaded);
    }

    #[test]
    fn each_missing_required_field_is_unreadable() {
        for key in REQUIRED {
            let error = parse(minimal(key).as_bytes()).unwrap_err();
            assert_eq!(error.reason, format!("missing required field: {key}"));
        }
    }

    #[test]
    fn newer_version_is_unreadable() {
        let text = minimal("").replace("inbox-version: 1", "inbox-version: 2");
        let error = parse(text.as_bytes()).unwrap_err();
        assert_eq!(error.reason, "newer item format (version 2)");
        let text = minimal("").replace("inbox-version: 1", "inbox-version: x");
        assert!(parse(text.as_bytes()).is_err());
    }

    #[test]
    fn bad_created_is_unreadable() {
        let text = minimal("").replace("2026-11-11T03:12:45Z", "yesterday");
        let error = parse(text.as_bytes()).unwrap_err();
        assert!(error.reason.contains("created"));
        assert!(error.line.is_some());
    }

    #[test]
    fn unknown_keys_kept_and_round_trip() {
        let text = PRD.replace("actions:", "flavor: mint (optional)\nactions:");
        let item = parse(text.as_bytes()).unwrap();
        assert_eq!(
            item.extra,
            vec![("flavor".to_string(), "mint (optional)".to_string())]
        );
        assert_eq!(parse(render(&item).as_bytes()).unwrap(), item);
    }

    #[test]
    fn empty_actions_and_round_trip() {
        let text = minimal("actions") + "";
        let text = text.replacen("---\n", "---\nactions: []\n", 1) + "line one\n\nline two";
        let item = parse(text.as_bytes()).unwrap();
        assert!(item.actions.is_empty());
        assert_eq!(parse(render(&item).as_bytes()).unwrap(), item);
        let item = parse(PRD.as_bytes()).unwrap();
        assert_eq!(parse(render(&item).as_bytes()).unwrap(), item);
    }

    #[test]
    fn run_target_accepted() {
        let text = PRD.replace("target: plan.md", "target: run:abc");
        let item = parse(text.as_bytes()).unwrap();
        assert_eq!(item.target.as_deref(), Some("run:abc"));
    }

    #[test]
    fn oversize_file_loads_header_only() {
        let mut text = PRD.to_string();
        text.push_str(&"x".repeat(MAX_FILE_BYTES));
        let item = parse(text.as_bytes()).unwrap();
        assert!(!item.body_loaded);
        assert!(item.body.is_empty());
        assert_eq!(item.summary, "Took the default for \"tab width\"");
    }

    #[test]
    fn unterminated_or_long_header_is_unreadable() {
        assert!(parse(b"---\nkind: a\n").is_err());
        assert!(parse(b"no header\n").is_err());
        let mut text = String::from("---\n");
        while text.len() <= MAX_HEADER_BYTES + 10 {
            text.push_str("junk: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
        }
        text.push_str("---\nbody\n");
        let error = parse(text.as_bytes()).unwrap_err();
        assert!(error.reason.contains("64 KiB"));
    }
}
