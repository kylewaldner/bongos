use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::*;

pub(crate) const FULL: &str = r#"
schema = 1
id = "0196f3a0-7c1e-7d2a-9b1e-2f4d5c6b7a80"
agent = "claude"
session_id = "8c1e4f8a-0000-4000-8000-000000000001"
type = "implement"
name = "add proration to renewal"
named_by = "kyle"
cwd = "/home/kyle/src/invoicing/.bongos-worktrees/billing-proration"
layout = "chat-only"
prompts_file = "/home/kyle/notes/prompts.txt"
model = "some-model"
parent = "0196f2aa-0000-7000-8000-000000000000"
fork = "handoff"
created = 2026-11-08T10:00:00Z
last_active = 2026-11-08T12:00:00Z
prompted = true
archived = true
archived_at = 2026-11-09T09:00:00Z
context_digest = "fnv1a64:9f3c000000000000"
context_given = 2026-11-08T12:00:00Z
"#;

const MINIMAL: &str = r#"
schema = 1
id = "0196f3a0-7c1e-7d2a-9b1e-2f4d5c6b7a81"
agent = "codex"
type = "free"
name = "free codex 2026-11-08"
cwd = "/tmp/x"
created = 2026-11-08T10:00:00Z
last_active = 2026-11-08T10:00:00Z
"#;

fn record_file(dir: &TempDir, id: &str, text: &str) -> PathBuf {
    let path = record_path(dir.path(), id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn paths_follow_the_scope_directory() {
    let scope = Path::new("/home/kyle/.bongos/projects/billing");
    assert_eq!(chats_dir(scope), scope.join("chats"));
    assert_eq!(record_path(scope, "abc"), scope.join("chats/abc.toml"));
}

#[test]
fn record_file_names() {
    assert!(is_record_file_name("0196.toml"));
    assert!(!is_record_file_name(".toml"));
    assert!(!is_record_file_name("notes.txt"));
    assert!(!is_record_file_name(".0196.toml.tmp-123-abcdefgh"));
    assert!(!is_record_file_name(".0196.tmp-123-abcdefgh.toml"));
}

#[test]
fn full_record_round_trips() {
    let record = parse_record(FULL).unwrap();
    assert_eq!(record.known_agent(), Some(Agent::Claude));
    assert_eq!(record.chat_type, "implement");
    assert_eq!(record.named_by, NamedBy::Kyle);
    assert_eq!(record.layout, Layout::ChatOnly);
    assert_eq!(record.fork, Some(Fork::Handoff));
    assert!(record.prompted && record.archived);
    assert!(record.extra.is_empty());

    let text = serialize_record(&record).unwrap();
    assert!(text.contains("type = \"implement\""));
    assert!(text.contains("created = 2026-11-08T10:00:00Z"));
    assert_eq!(parse_record(&text).unwrap(), record);
}

#[test]
fn minimal_record_gets_defaults_and_omits_absent_keys() {
    let record = parse_record(MINIMAL).unwrap();
    assert_eq!(record.named_by, NamedBy::Auto);
    assert_eq!(record.layout, Layout::Dual);
    assert!(!record.prompted && !record.archived);
    assert_eq!(record.session_id, None);

    let text = serialize_record(&record).unwrap();
    for absent in [
        "session_id",
        "prompts_file",
        "model",
        "parent",
        "fork",
        "archived_at",
    ] {
        assert!(!text.contains(absent), "{absent} in {text}");
    }
    assert_eq!(parse_record(&text).unwrap(), record);
}

#[test]
fn unknown_keys_survive_a_rewrite() {
    let text = format!(
        "{MINIMAL}auto_submit = \"off\"\nretries = 3\n\n[feature25]\nlast_block = 7\nnames = [\"a\", \"b\"]\n"
    );
    let record = parse_record(&text).unwrap();
    assert_eq!(record.extra.len(), 3);

    let mut changed = record.clone();
    changed.name = "renamed".to_string();
    let rewritten = serialize_record(&changed).unwrap();
    let again = parse_record(&rewritten).unwrap();
    assert_eq!(again.name, "renamed");
    assert_eq!(again.extra, record.extra);
    assert!(rewritten.contains("auto_submit = \"off\""));
    assert!(rewritten.contains("last_block = 7"));
}

#[test]
fn unknown_keys_survive_update_on_disk() {
    let dir = TempDir::new().unwrap();
    let path = record_file(&dir, "a", &format!("{MINIMAL}extra_key = \"kept\"\n"));
    update_record(&path, |record| {
        record.prompted = true;
        Ok(())
    })
    .unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("extra_key = \"kept\""));
    assert!(text.contains("prompted = true"));
}

#[test]
fn an_unknown_agent_still_parses() {
    let record = parse_record(&MINIMAL.replace("codex", "gemini")).unwrap();
    assert_eq!(record.agent, "gemini");
    assert_eq!(record.known_agent(), None);
}

#[test]
fn syntax_error_reports_file_line_and_column() {
    let dir = TempDir::new().unwrap();
    let path = record_file(
        &dir,
        "bad",
        "schema = 1\nid = \"x\"\nagent = = \"claude\"\n",
    );
    let error = read_record(&path).unwrap_err();
    assert_eq!(error.path.as_deref(), Some(path.as_path()));
    assert_eq!(error.line, Some(3));
    assert!(error.column.is_some());
    assert!(matches!(error.kind, ErrorKind::Syntax(_)));
    assert!(
        error
            .to_string()
            .contains(&format!("{}:3:", path.display()))
    );
}

#[test]
fn missing_field_is_reported_with_the_file() {
    let dir = TempDir::new().unwrap();
    let path = record_file(&dir, "short", "schema = 1\nid = \"x\"\n");
    let error = read_record(&path).unwrap_err();
    assert!(error.to_string().contains("missing field"), "{error}");
    assert_eq!(error.path.as_deref(), Some(path.as_path()));
}

#[test]
fn schema_is_required_and_checked() {
    let missing = parse_record(&MINIMAL.replace("schema = 1", "")).unwrap_err();
    assert!(matches!(
        missing.kind,
        ErrorKind::UnsupportedSchema { found: None }
    ));
    let other = parse_record(&MINIMAL.replace("schema = 1", "schema = 2")).unwrap_err();
    assert!(other.to_string().contains("unsupported schema 2"));

    let mut record = parse_record(MINIMAL).unwrap();
    record.schema = 2;
    assert!(serialize_record(&record).is_err());
}

#[test]
fn invalid_file_is_not_rewritten_by_update() {
    let dir = TempDir::new().unwrap();
    let broken = "schema = 1\nid = \n";
    let path = record_file(&dir, "bad", broken);
    let result = update_record(&path, |record| {
        record.archived = true;
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
    assert_eq!(leftovers, 1);
}

#[test]
fn update_refuses_a_missing_file() {
    let dir = TempDir::new().unwrap();
    let path = record_path(dir.path(), "gone");
    let error = update_record(&path, |_| Ok(())).unwrap_err();
    assert!(matches!(error.kind, ErrorKind::Io(_)));
    assert!(!path.exists());
}

#[test]
fn failed_change_leaves_the_file_alone() {
    let dir = TempDir::new().unwrap();
    let path = record_file(&dir, "a", MINIMAL);
    let result = update_record(&path, |record| {
        record.name = "changed".to_string();
        Err(Error::io(Path::new("x"), std::io::Error::other("stop")))
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), MINIMAL);
}

#[test]
fn create_writes_into_the_chats_directory_once() {
    let dir = TempDir::new().unwrap();
    let record = ChatRecord::new(Agent::Claude, "plan", PathBuf::from("/tmp/work"));
    let path = create_record(dir.path(), &record).unwrap();
    assert_eq!(path, record_path(dir.path(), &record.id));
    assert_eq!(read_record(&path).unwrap(), record);

    let again = create_record(dir.path(), &record).unwrap_err();
    assert!(
        matches!(&again.kind, ErrorKind::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists)
    );
}

#[cfg(unix)]
#[test]
fn new_files_are_private_and_rewrites_keep_the_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let record = ChatRecord::new(Agent::Codex, "free", PathBuf::from("/tmp/work"));
    let path = create_record(dir.path(), &record).unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    update_record(&path, |record| {
        record.prompted = true;
        Ok(())
    })
    .unwrap();
    assert_eq!(mode(&path), 0o644);
}

#[test]
fn new_record_defaults() {
    let record = ChatRecord::new(Agent::Claude, "implement", PathBuf::from("/tmp/work"));
    assert_eq!(record.schema, 1);
    assert_eq!(record.known_agent(), Some(Agent::Claude));
    assert_eq!(record.named_by, NamedBy::None);
    assert!(record.name.starts_with("implement claude "));
    assert!(!record.prompted && !record.archived);
    assert_eq!(record.created, record.last_active);
    assert_eq!(record.id.len(), 36);
}

#[test]
fn auto_name_never_overwrites_a_name_kyle_chose() {
    let mut record = parse_record(FULL).unwrap();
    assert!(!record.apply_auto_name("from a prompt".to_string()));
    assert_eq!(record.name, "add proration to renewal");

    record.named_by = NamedBy::None;
    assert!(record.apply_auto_name("from a prompt".to_string()));
    assert_eq!(record.name, "from a prompt");
    assert_eq!(record.named_by, NamedBy::Auto);
}

#[test]
fn timestamps_order_across_offsets() {
    let utc = timestamp_utc(&"2026-11-08T12:00:00Z".parse().unwrap()).unwrap();
    let offset = timestamp_utc(&"2026-11-08T13:00:00+02:00".parse().unwrap()).unwrap();
    let local = timestamp_utc(&"2026-11-08T12:00:00".parse().unwrap()).unwrap();
    assert!(offset < utc);
    assert_eq!(local, utc);
    assert_eq!(timestamp_utc(&"2026-11-08".parse().unwrap()), None);
}
