use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::record_tests::FULL;
use crate::*;

fn entry_ids(entries: &[&ChatEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id()).collect()
}

fn write_chat(scope: &Path, id: &str, last_active: &str, archived: bool, parent: Option<&str>) {
    let mut record = parse_record(FULL).unwrap();
    record.id = id.to_string();
    record.name = format!("chat {id}");
    record.last_active = last_active.parse().unwrap();
    record.archived = archived;
    record.archived_at = None;
    record.parent = parent.map(str::to_string);
    fs::create_dir_all(chats_dir(scope)).unwrap();
    write_record(&record_path(scope, id), &record).unwrap();
}

#[test]
fn a_scope_without_chats_is_empty() {
    let dir = TempDir::new().unwrap();
    assert!(read_scope_chats(dir.path()).unwrap().is_empty());
    fs::create_dir(chats_dir(dir.path())).unwrap();
    assert!(read_scope_chats(dir.path()).unwrap().is_empty());
}

#[test]
fn open_and_archived_lists_are_most_recent_first() {
    let dir = TempDir::new().unwrap();
    write_chat(dir.path(), "a", "2026-11-08T10:00:00Z", false, None);
    write_chat(dir.path(), "b", "2026-11-08T12:00:00Z", false, None);
    write_chat(dir.path(), "c", "2026-11-08T11:00:00Z", false, None);
    write_chat(dir.path(), "d", "2026-11-07T11:00:00Z", true, None);
    write_chat(dir.path(), "e", "2026-11-09T11:00:00Z", true, None);

    let entries = read_scope_chats(dir.path()).unwrap();
    assert_eq!(entries.len(), 5);
    let lists = chat_lists(&entries);
    assert_eq!(entry_ids(&lists.open), ["b", "c", "a"]);
    assert_eq!(entry_ids(&lists.archived), ["e", "d"]);
}

#[test]
fn order_compares_instants_and_breaks_ties_by_id() {
    let dir = TempDir::new().unwrap();
    // 13:00 at +02:00 is 11:00 UTC, older than 11:30 UTC.
    write_chat(dir.path(), "zone", "2026-11-08T13:00:00+02:00", false, None);
    write_chat(dir.path(), "utc", "2026-11-08T11:30:00Z", false, None);
    write_chat(dir.path(), "tie-b", "2026-11-08T09:00:00Z", false, None);
    write_chat(dir.path(), "tie-a", "2026-11-08T09:00:00Z", false, None);

    let entries = read_scope_chats(dir.path()).unwrap();
    let lists = chat_lists(&entries);
    assert_eq!(entry_ids(&lists.open), ["utc", "zone", "tie-a", "tie-b"]);
}

#[test]
fn invalid_files_are_listed_last_with_their_error_and_left_alone() {
    let dir = TempDir::new().unwrap();
    write_chat(dir.path(), "good", "2026-11-08T10:00:00Z", false, None);
    let broken = chats_dir(dir.path()).join("broken.toml");
    fs::write(&broken, "schema = 1\nid = \"x\"\nagent = = 1\n").unwrap();
    let archived_looking = chats_dir(dir.path()).join("also-broken.toml");
    fs::write(&archived_looking, "archived = true\n").unwrap();

    let entries = read_scope_chats(dir.path()).unwrap();
    let lists = chat_lists(&entries);
    assert_eq!(entry_ids(&lists.open), ["good", "also-broken", "broken"]);
    assert!(lists.archived.is_empty());

    let invalid = lists.open[2];
    assert_eq!(invalid.file_name(), "broken.toml");
    let error = invalid.chat.as_ref().unwrap_err();
    assert_eq!(error.line, Some(3));
    assert_eq!(
        fs::read_to_string(&broken).unwrap(),
        "schema = 1\nid = \"x\"\nagent = = 1\n"
    );
}

#[test]
fn other_files_in_the_chats_directory_are_skipped() {
    let dir = TempDir::new().unwrap();
    write_chat(dir.path(), "a", "2026-11-08T10:00:00Z", false, None);
    let chats = chats_dir(dir.path());
    fs::write(chats.join("notes.txt"), "x").unwrap();
    fs::write(chats.join(".a.toml.tmp-1-abcdefgh"), "x").unwrap();
    fs::create_dir(chats.join("sub.toml")).unwrap();

    let entries = read_scope_chats(dir.path()).unwrap();
    assert_eq!(entries.len(), 1);
}

#[test]
fn forks_are_found_by_parent() {
    let dir = TempDir::new().unwrap();
    write_chat(dir.path(), "root", "2026-11-08T08:00:00Z", false, None);
    write_chat(
        dir.path(),
        "f1",
        "2026-11-08T09:00:00Z",
        false,
        Some("root"),
    );
    write_chat(dir.path(), "f2", "2026-11-08T10:00:00Z", true, Some("root"));
    write_chat(
        dir.path(),
        "grandchild",
        "2026-11-08T11:00:00Z",
        false,
        Some("f1"),
    );
    write_chat(
        dir.path(),
        "other",
        "2026-11-08T12:00:00Z",
        false,
        Some("elsewhere"),
    );

    let entries = read_scope_chats(dir.path()).unwrap();
    assert_eq!(entry_ids(&forks_of(&entries, "root")), ["f2", "f1"]);
    assert_eq!(entry_ids(&forks_of(&entries, "f1")), ["grandchild"]);
    assert!(forks_of(&entries, "grandchild").is_empty());
    assert!(forks_of(&entries, "missing").is_empty());
}

#[test]
fn lists_work_across_scopes() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    write_chat(first.path(), "a", "2026-11-08T10:00:00Z", false, None);
    write_chat(second.path(), "b", "2026-11-08T11:00:00Z", false, Some("a"));

    let mut entries = read_scope_chats(first.path()).unwrap();
    entries.extend(read_scope_chats(second.path()).unwrap());
    assert_eq!(entry_ids(&chat_lists(&entries).open), ["b", "a"]);
    assert_eq!(entry_ids(&forks_of(&entries, "a")), ["b"]);
}
