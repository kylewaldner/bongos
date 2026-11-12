use std::path::PathBuf;

use tempfile::TempDir;

use crate::{Location, RepositoryLocations};

fn location(id: &str) -> Location {
    let folder = PathBuf::from(format!("/data/projects/{id}"));
    Location {
        id: id.to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        state: folder.join("state"),
        files: folder.join("files"),
        folder,
        remote: None,
        converted_from: None,
    }
}

#[test]
fn a_missing_record_is_empty() {
    let dir = TempDir::new().unwrap();
    let locations = RepositoryLocations::load(dir.path().join("project-repositories.toml")).unwrap();
    assert!(locations.locations().is_empty());
    assert!(locations.location("x").is_none());
}

#[test]
fn the_record_round_trips() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state").join("project-repositories.toml");
    let mut locations = RepositoryLocations::load(&path).unwrap();
    let mut with_remote = location("b");
    with_remote.remote = Some("git@example.com:me/b.git".to_string());
    with_remote.converted_from = Some(PathBuf::from("/old/b"));
    locations.record(location("a")).unwrap();
    locations.record(with_remote.clone()).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("schema = 1\n"), "{text}");
    assert_eq!(text.matches("[[project]]").count(), 2);

    let reread = RepositoryLocations::load(&path).unwrap();
    assert_eq!(reread.locations(), &[location("a"), with_remote.clone()]);
    assert_eq!(reread.location("b"), Some(&with_remote));

    // Recording the same id again replaces it.
    let mut moved = location("a");
    moved.remote = Some("origin".to_string());
    let mut locations = reread;
    locations.record(moved.clone()).unwrap();
    let reread = RepositoryLocations::load(&path).unwrap();
    assert_eq!(reread.locations().len(), 2);
    assert_eq!(reread.location("a"), Some(&moved));
}

#[test]
fn an_unknown_schema_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("project-repositories.toml");
    std::fs::write(&path, "schema = 2\n").unwrap();
    assert!(RepositoryLocations::load(&path).is_err());
}
