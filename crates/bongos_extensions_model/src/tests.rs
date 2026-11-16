use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

fn embedded() -> LanguageList {
    LanguageList::embedded().unwrap()
}

#[test]
fn embedded_list_is_well_formed() {
    let list = embedded();
    assert_eq!(list.entries().len(), 18);
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for entry in list.entries() {
        assert!(ids.insert(entry.id.clone()), "duplicate id {}", entry.id);
        assert!(
            names.insert(entry.name.to_ascii_lowercase()),
            "duplicate name {}",
            entry.name
        );
        assert_eq!(entry.commit.len(), 40, "{}", entry.id);
        assert!(entry.commit.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(entry.repository.starts_with("https://"), "{}", entry.id);
        let url = SourceUrl::parse(&entry.repository).unwrap();
        assert!(!bongos_net::is_blocked_host(url.host().unwrap()));
        if let Some(path) = &entry.path {
            validate_relative_path(path).unwrap();
        }
        assert!(!entry.description.is_empty());
    }
}

fn built_in_languages() -> Vec<(String, Vec<String>)> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut languages = Vec::new();
    for dir in ["../grammars/src", "../bongos_languages/src"] {
        let dir = manifest.join(dir);
        if !dir.exists() {
            assert!(dir.ends_with("bongos_languages/src"), "{dir:?} is missing");
            continue;
        }
        for item in std::fs::read_dir(&dir).unwrap() {
            let config = item.unwrap().path().join("config.toml");
            let Ok(text) = std::fs::read_to_string(&config) else {
                continue;
            };
            let value: toml::Table = toml::from_str(&text).unwrap();
            let name = value["name"].as_str().unwrap().to_string();
            let suffixes = value
                .get("path_suffixes")
                .and_then(|suffixes| suffixes.as_array())
                .map(|suffixes| {
                    suffixes
                        .iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            languages.push((name, suffixes));
        }
    }
    assert!(!languages.is_empty());
    languages
}

#[test]
fn list_does_not_overlap_built_in_languages() {
    let built_in = built_in_languages();
    for entry in embedded().entries() {
        for (name, suffixes) in &built_in {
            assert!(
                !name.eq_ignore_ascii_case(&entry.name),
                "{} is a built-in language",
                entry.name
            );
            for file_type in &entry.file_types {
                assert!(
                    !suffixes.contains(file_type),
                    "{} lists `{file_type}`, which {name} already handles",
                    entry.name
                );
            }
        }
    }
}

#[test]
fn entry_for_path_matches_suffixes_and_names() {
    let list = embedded();
    let id = |path: &str| {
        list.entry_for_path(&PathBuf::from(path))
            .map(|e| e.id.as_str())
    };
    assert_eq!(id("src/main.swift"), Some("swift"));
    assert_eq!(id("Dockerfile"), Some("dockerfile"));
    assert_eq!(id("app/Dockerfile"), Some("dockerfile"));
    // Zed's language registry compares suffixes case-sensitively, so this does too.
    assert_eq!(id("a.SWIFT"), None);
    assert_eq!(id("x.unlistedsuffix"), None);
    assert_eq!(list.entry_by_name("swift").unwrap().id, "swift");
    assert_eq!(list.entry_by_name("C#").unwrap().id, "csharp");
    assert!(list.entry_by_name("nope").is_none());
}

#[test]
fn source_url_forms() {
    for ok in [
        "https://github.com/a/b",
        "https://github.com/a/b.git",
        "ssh://git@github.com/a/b.git",
        "git@github.com:a/b.git",
        "file:///home/me/repo",
    ] {
        SourceUrl::parse(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    assert_eq!(
        SourceUrl::parse("http://github.com/a/b"),
        Err(SourceUrlError::InsecureScheme)
    );
    for bad in [
        "",
        "ftp://github.com/a/b",
        "garbage",
        "https:///a/b",
        "https://github.com",
        "https://user:pw@github.com/a/b",
        "file://relative/path",
        "git://github.com/a/b",
    ] {
        assert!(SourceUrl::parse(bad).is_err(), "{bad} should be refused");
    }
    assert!(validate_relative_path("extensions/proto").is_ok());
    assert!(validate_relative_path("../x").is_err());
    assert!(validate_relative_path("/abs").is_err());
}

#[test]
fn source_urls_normalize() {
    let a = SourceUrl::parse("https://GitHub.com/a/b.git/").unwrap();
    let b = SourceUrl::parse("https://github.com/a/b").unwrap();
    assert_eq!(a, b);
    assert_eq!(a.as_str(), "https://github.com/a/b");
    assert_eq!(a.host(), Some("github.com"));
    assert_eq!(
        SourceUrl::parse("git@GitHub.com:a/b.git").unwrap(),
        SourceUrl::parse("git@github.com:a/b").unwrap()
    );
    assert_ne!(a, SourceUrl::parse("https://github.com/a/c").unwrap());
}

#[test]
fn short_hashes_are_refused() {
    let full = "a".repeat(40);
    assert_eq!(
        RevSpec::parse(Some(&full)),
        Ok(RevSpec::Commit(full.clone()))
    );
    assert_eq!(
        RevSpec::parse(Some(&"A".repeat(40))),
        Ok(RevSpec::Commit(full))
    );
    assert!(RevSpec::parse(Some(&"b".repeat(64))).is_ok());
    for short in ["abcd", "deadbeef", &"a".repeat(39), &"a".repeat(41)] {
        let error = RevSpec::parse(Some(short)).unwrap_err();
        assert!(error.to_string().contains("full commit"), "{error}");
    }
    assert_eq!(
        RevSpec::parse(Some("v1.2.0")),
        Ok(RevSpec::Name("v1.2.0".into()))
    );
    assert_eq!(
        RevSpec::parse(Some("main")),
        Ok(RevSpec::Name("main".into()))
    );
    assert_eq!(RevSpec::parse(None), Ok(RevSpec::Default));
    assert_eq!(RevSpec::parse(Some("")), Ok(RevSpec::Default));
}

#[test]
fn is_listed_requires_the_listed_commit() {
    let list = embedded();
    let swift = list.entry_by_name("Swift").unwrap();
    let url = SourceUrl::parse("https://GitHub.com/samuser107/zed-swift-extension.git").unwrap();
    assert!(list.is_listed(&url, None, &swift.commit));
    assert!(!list.is_listed(&url, None, &"0".repeat(40)));
    assert!(!list.is_listed(&url, Some("sub"), &swift.commit));
    let proto = list.entry_by_name("Protobuf").unwrap();
    let zed = SourceUrl::parse(&proto.repository).unwrap();
    assert!(list.is_listed(&zed, proto.path.as_deref(), &proto.commit));
    assert!(!list.is_listed(&zed, None, &proto.commit));
}
