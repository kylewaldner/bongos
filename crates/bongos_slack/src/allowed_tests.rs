//! Tests of the writers of `allowed.toml` (`slack: allow channel`, `slack: disallow channel`),
//! each in its own temporary directory.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use bongos_slack_model::ConversationId;

use crate::allowed::{
    add_in, add_with, allow_in, allowed_entries, disallow_in, load_in, names_in, person_of,
    remove_in, remove_with,
};
use crate::connection::allowed_path;
use crate::read_buffer::ConversationEntry;

fn id(text: &str) -> ConversationId {
    ConversationId(text.to_string())
}

fn entry(label: &str, conversation: &str) -> ConversationEntry {
    ConversationEntry {
        label: label.to_string(),
        id: id(conversation),
    }
}

fn listed(dir: &Path, project: &str) -> Vec<String> {
    load_in(dir, project)
        .iter()
        .map(|conversation| conversation.to_string())
        .collect()
}

fn failing_write(_: &Path, _: &str, _: &[u8]) -> io::Result<()> {
    Err(io::Error::other("disk full"))
}

#[test]
fn allow_then_disallow_round_trips_the_file() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("slack");
    assert!(listed(&dir, "p1").is_empty(), "no file is an empty list");

    let builds = entry("#team-builds", "C0BUILDS");
    let message = allow_in(&dir, "p1", &builds);
    assert_eq!(
        message,
        "Runs and chat agents of p1 may post to #team-builds without asking"
    );
    assert_eq!(listed(&dir, "p1"), vec!["C0BUILDS"]);
    let text = fs::read_to_string(allowed_path(&dir)).unwrap();
    assert!(text.contains("[projects.p1]"), "{text}");
    assert!(text.contains("conversations = [\"C0BUILDS\"]"), "{text}");
    assert_eq!(
        allow_in(&dir, "p1", &builds),
        "#team-builds was already allowed for p1"
    );

    assert_eq!(
        disallow_in(&dir, "p1", &builds),
        "Runs and chat agents of p1 ask again before posting to #team-builds"
    );
    assert!(listed(&dir, "p1").is_empty());
    assert_eq!(
        disallow_in(&dir, "p1", &builds),
        "#team-builds was not allowed for p1"
    );
    // The file is still valid and readable, with nothing listed.
    let text = fs::read_to_string(allowed_path(&dir)).unwrap();
    assert!(!text.contains("C0BUILDS"), "{text}");
}

#[test]
fn dm_entry_names_the_person() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().to_path_buf();
    let dm = entry("@ana", "D0ANA");
    assert_eq!(person_of(&dm), Some("ana".to_string()));
    assert_eq!(person_of(&entry("#team-builds", "C0BUILDS")), None);

    allow_in(&dir, "p1", &dm);

    // Only this conversation is allowed, never every direct message.
    assert_eq!(listed(&dir, "p1"), vec!["D0ANA"]);
    assert_eq!(
        names_in(&dir, "p1"),
        BTreeMap::from([(id("D0ANA"), "ana".to_string())])
    );
    // Without the conversation list, the picker of `slack: disallow channel` still names her.
    let entries = allowed_entries(&load_in(&dir, "p1"), &names_in(&dir, "p1"), &[]);
    assert_eq!(entries, vec![entry("@ana", "D0ANA")]);

    remove_in(&dir, "p1", &id("D0ANA")).unwrap();
    assert!(names_in(&dir, "p1").is_empty(), "the name goes with it");
}

#[test]
fn allowed_list_is_per_project() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().to_path_buf();

    assert!(add_in(&dir, "project-a", &id("C0A"), None).unwrap());
    assert!(add_in(&dir, "project-b", &id("C0B"), None).unwrap());
    assert!(add_in(&dir, "project-b", &id("D0ANA"), Some("ana")).unwrap());

    assert_eq!(listed(&dir, "project-a"), vec!["C0A"]);
    assert_eq!(listed(&dir, "project-b"), vec!["C0B", "D0ANA"]);
    assert!(names_in(&dir, "project-a").is_empty());

    // Taking a conversation off project A keeps project B's entries.
    assert!(remove_in(&dir, "project-a", &id("C0A")).unwrap());
    assert!(!remove_in(&dir, "project-a", &id("C0B")).unwrap());
    assert!(listed(&dir, "project-a").is_empty());
    assert_eq!(listed(&dir, "project-b"), vec!["C0B", "D0ANA"]);
    assert_eq!(names_in(&dir, "project-b").len(), 1);
}

#[test]
fn failed_write_leaves_the_old_file() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().to_path_buf();
    add_in(&dir, "p1", &id("C0A"), None).unwrap();
    let before = fs::read_to_string(allowed_path(&dir)).unwrap();

    let error = add_with(&dir, "p1", &id("C0B"), None, failing_write).unwrap_err();
    assert!(error.to_string().contains("it was not changed"), "{error}");
    let error = remove_with(&dir, "p1", &id("C0A"), failing_write).unwrap_err();
    assert!(error.to_string().contains("disk full"), "{error}");

    assert_eq!(fs::read_to_string(allowed_path(&dir)).unwrap(), before);
    assert_eq!(listed(&dir, "p1"), vec!["C0A"]);
}

#[test]
fn invalid_file_is_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().to_path_buf();
    fs::write(allowed_path(&dir), "projects = [not toml").unwrap();

    let message = allow_in(&dir, "p1", &entry("#team-builds", "C0BUILDS"));

    assert!(message.contains("not valid"), "{message}");
    assert_eq!(
        fs::read_to_string(allowed_path(&dir)).unwrap(),
        "projects = [not toml"
    );
}

#[test]
fn write_goes_through_a_renamed_temporary_and_stays_readable_by_the_old_format() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().to_path_buf();
    // A file in the format before names were kept.
    fs::write(
        allowed_path(&dir),
        "[projects.\"p0\"]\nconversations = [\"C0OLD\"]\n",
    )
    .unwrap();

    add_in(&dir, "p1", &id("D0ANA"), Some("ana")).unwrap();

    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["allowed.toml"], "no temporary file is left");
    assert_eq!(listed(&dir, "p0"), vec!["C0OLD"], "other projects are kept");
    assert_eq!(listed(&dir, "p1"), vec!["D0ANA"]);
    // A reader that knows only `conversations` still reads the new file.
    #[derive(serde::Deserialize)]
    struct OldEntry {
        conversations: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct OldFile {
        projects: BTreeMap<String, OldEntry>,
    }
    let old: OldFile = toml::from_str(&fs::read_to_string(allowed_path(&dir)).unwrap()).unwrap();
    assert_eq!(old.projects["p1"].conversations, vec!["D0ANA"]);
}
