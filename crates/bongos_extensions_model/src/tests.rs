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

#[test]
fn embedded_apis_are_within_the_stable_limit() {
    let mut file_types = HashSet::new();
    for entry in embedded().entries() {
        if let Some(api) = &entry.api {
            let parts: Vec<u64> = api
                .split('.')
                .map(|part| {
                    part.parse()
                        .unwrap_or_else(|_| panic!("{}: {api}", entry.id))
                })
                .collect();
            assert_eq!(parts.len(), 3, "{}: {api}", entry.id);
            assert!(
                (parts[0], parts[1], parts[2]) <= (0, 7, 0),
                "{} needs api {api}, newer than 0.7.0",
                entry.id
            );
        }
        for file_type in &entry.file_types {
            assert!(
                file_types.insert(file_type.clone()),
                "file type {file_type} is listed twice"
            );
        }
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

mod install_state {
    use super::super::*;
    use std::fs;

    fn sample_record() -> InstallRecord {
        InstallRecord {
            version: 1,
            id: "kotlin".into(),
            source_url: "https://example.com/org/kotlin".into(),
            path: Some("ext".into()),
            requested_ref: Some("main".into()),
            commit: "a".repeat(40),
            listed: true,
            trusted_commit: None,
            state: RecordState::Installed,
            installed_at: "2026-01-01T00:00:00Z".into(),
            extra: Default::default(),
        }
    }

    #[test]
    fn record_round_trips_and_keeps_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kotlin").join("record.json");
        let record = sample_record();
        write_atomic(&path, &record, "n1").unwrap();
        assert_eq!(read(&path).unwrap(), ReadOutcome::Ok(record));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["future_field"] = serde_json::json!({"a": [1, 2]});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let ReadOutcome::Ok(read_back) = read(&path).unwrap() else {
            panic!("record should be readable");
        };
        assert!(read_back.extra.contains_key("future_field"));
        write_atomic(&path, &read_back, "n2").unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(rewritten["future_field"], value["future_field"]);
    }

    #[test]
    fn torn_temp_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        fs::write(dir.path().join("record.json.tmp-x"), b"{\"version\":").unwrap();
        assert_eq!(read(&path).unwrap(), ReadOutcome::Missing);
        write_atomic(&path, &sample_record(), "y").unwrap();
        assert!(matches!(read(&path).unwrap(), ReadOutcome::Ok(_)));
        remove_torn_temp_files(dir.path()).unwrap();
        assert!(!dir.path().join("record.json.tmp-x").exists());
        assert!(path.exists());
    }

    #[test]
    fn unreadable_record_is_renamed_bad() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        let bad = dir.path().join("record.json.bad");
        fs::write(&bad, b"old").unwrap();
        fs::write(&path, b"not json").unwrap();
        assert_eq!(read(&path).unwrap(), ReadOutcome::Unreadable);
        assert!(!path.exists());
        assert_eq!(fs::read(&bad).unwrap(), b"not json");
        let mut newer = sample_record();
        newer.version = 2;
        fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
        assert_eq!(read(&path).unwrap(), ReadOutcome::Unreadable);
    }

    #[test]
    fn layout_paths_and_ids() {
        let layout = Layout::new("/x".into());
        assert_eq!(
            layout.rebuild_checkout_dir("k", "c", "n").to_str().unwrap(),
            "/x/git/k/c.rebuild-n"
        );
        assert_eq!(layout.symlink_path("k").to_str().unwrap(), "/x/installed/k");
        let url = SourceUrl::parse("https://Example.com/a/b.git").unwrap();
        let same = SourceUrl::parse("https://example.com/a/b").unwrap();
        let key = layout.source_key(&url);
        assert_eq!(key, layout.source_key(&same));
        assert!(key.starts_with("source-") && key.len() == 15);
        assert!(validate_id("kotlin-2_x").is_ok());
        for bad in ["", "staging", "Logs", "LOCKS", ".", "..", "a/b"] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    fn rec(id: &str, commit: &str, state: RecordState) -> RecordSnapshot {
        let mut r = sample_record();
        r.id = id.into();
        r.commit = commit.into();
        r.state = state;
        RecordSnapshot::Some(r)
    }

    fn ext(
        id: &str,
        checkouts: &[(&str, bool)],
        record: RecordSnapshot,
        lock: bool,
    ) -> ExtensionSnapshot {
        ExtensionSnapshot {
            id: id.into(),
            checkouts: checkouts
                .iter()
                .map(|(f, m)| CheckoutSnapshot {
                    folder: (*f).into(),
                    has_manifest: *m,
                })
                .collect(),
            record,
            lock_held: lock,
        }
    }

    fn link(id: &str, commit: &str) -> SymlinkSnapshot {
        SymlinkSnapshot {
            id: id.into(),
            target: SymlinkTarget::Checkout {
                id: id.into(),
                commit: commit.into(),
                origin_url: Some("https://example.com/o".into()),
            },
        }
    }

    fn snap(extensions: Vec<ExtensionSnapshot>, symlinks: Vec<SymlinkSnapshot>) -> Snapshot {
        Snapshot {
            git_dir_exists: true,
            staging: Vec::new(),
            extensions,
            symlinks,
            locked_ids_without_folder: Vec::new(),
        }
    }

    fn del(id: &str, folder: &str) -> Fix {
        Fix::DeleteCheckout {
            id: id.into(),
            folder: folder.into(),
        }
    }

    fn wr(id: &str, commit: &str) -> Fix {
        Fix::WriteRecordFromSymlink {
            id: id.into(),
            commit: commit.into(),
            origin_url: Some("https://example.com/o".into()),
        }
    }

    #[test]
    fn plan_first_use_and_staging_swept() {
        let mut s = snap(vec![], vec![]);
        s.git_dir_exists = false;
        s.staging = vec!["a".into()];
        assert!(plan(&s).is_empty());
        s.git_dir_exists = true;
        s.staging = vec!["b".into(), "a".into()];
        s.extensions = vec![ext("k", &[], RecordSnapshot::None, false)];
        s.symlinks = vec![SymlinkSnapshot {
            id: "other".into(),
            target: SymlinkTarget::Elsewhere,
        }];
        assert_eq!(
            plan(&s),
            vec![
                Fix::DeleteStaging { nonce: "a".into() },
                Fix::DeleteStaging { nonce: "b".into() }
            ]
        );
    }

    #[test]
    fn plan_crash_after_build_before_link() {
        let s = snap(
            vec![ext("k", &[("c1", true)], RecordSnapshot::None, false)],
            vec![],
        );
        assert_eq!(plan(&s), vec![del("k", "c1")]);
    }

    #[test]
    fn plan_crash_after_link_before_record() {
        let s = snap(
            vec![ext(
                "k",
                &[("c1", true), ("c0", true)],
                RecordSnapshot::None,
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![del("k", "c0"), wr("k", "c1")]);
        // A stale record naming another commit is replaced the same way.
        let s = snap(
            vec![ext(
                "k",
                &[("c1", true), ("c0", true)],
                rec("k", "c0", RecordState::Installed),
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![del("k", "c0"), wr("k", "c1")]);
        // An unreadable record is rewritten from the link and the linked checkout is kept.
        let s = snap(
            vec![ext("k", &[("c1", true)], RecordSnapshot::Unreadable, false)],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![wr("k", "c1")]);
    }

    #[test]
    fn plan_crash_during_update_swap() {
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true), ("c1", true)],
                rec("k", "c0", RecordState::Installed),
                false,
            )],
            vec![],
        );
        assert_eq!(
            plan(&s),
            vec![
                del("k", "c1"),
                Fix::Relink {
                    id: "k".into(),
                    commit: "c0".into()
                }
            ]
        );
        for checkouts in [&[("c0", false)][..], &[][..]] {
            let s = snap(
                vec![ext(
                    "k",
                    checkouts,
                    rec("k", "c0", RecordState::Installed),
                    false,
                )],
                vec![],
            );
            assert_eq!(plan(&s), vec![Fix::MarkBuildMissing { id: "k".into() }]);
        }
    }

    #[test]
    fn plan_record_removing() {
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true), ("c1", true)],
                rec("k", "c0", RecordState::Removing),
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![Fix::FinishRemoval { id: "k".into() }]);
    }

    #[test]
    fn plan_lock_held() {
        let mut s = snap(
            vec![
                ext("k", &[("c0", true)], RecordSnapshot::None, true),
                ext("m", &[("c9", true)], RecordSnapshot::None, false),
            ],
            vec![],
        );
        s.staging = vec!["n".into()];
        assert_eq!(plan(&s), vec![del("m", "c9")]);
        s.extensions[0].lock_held = false;
        assert_eq!(
            plan(&s),
            vec![
                Fix::DeleteStaging { nonce: "n".into() },
                del("k", "c0"),
                del("m", "c9")
            ]
        );
    }

    #[test]
    fn plan_skips_reserved_and_invalid_ids() {
        let s = snap(
            vec![
                ext("logs", &[("c", true)], RecordSnapshot::None, false),
                ext("LOCKS", &[("c", true)], RecordSnapshot::None, false),
                ext("a/b", &[("c", true)], RecordSnapshot::None, false),
            ],
            vec![link("Staging", "c"), link("logs", "c")],
        );
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn plan_link_to_another_ids_checkout() {
        let mut foreign = link("foo", "c");
        foreign.target = SymlinkTarget::Checkout {
            id: "bar".into(),
            commit: "c".into(),
            origin_url: None,
        };
        let s = snap(
            vec![ext(
                "bar",
                &[("c", true), ("d", true)],
                RecordSnapshot::None,
                false,
            )],
            vec![foreign],
        );
        assert_eq!(plan(&s), vec![del("bar", "d")]);
    }

    #[test]
    fn plan_orphan_link_marks_build_missing() {
        let s = snap(vec![], vec![link("k", "c1")]);
        assert_eq!(plan(&s), vec![Fix::MarkBuildMissing { id: "k".into() }]);
    }

    #[test]
    fn plan_unreadable_record_with_link_writes_record() {
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true), ("c1", true)],
                RecordSnapshot::Unreadable,
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![del("k", "c0"), wr("k", "c1")]);
    }

    #[test]
    fn plan_dangling_link_keeps_the_recorded_checkout() {
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true)],
                rec("k", "c0", RecordState::Installed),
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(
            plan(&s),
            vec![Fix::Relink {
                id: "k".into(),
                commit: "c0".into()
            }]
        );
        let s = snap(
            vec![ext(
                "k",
                &[("c0", false)],
                rec("k", "c0", RecordState::Installed),
                false,
            )],
            vec![link("k", "c1")],
        );
        assert_eq!(plan(&s), vec![Fix::MarkBuildMissing { id: "k".into() }]);
        let s = snap(
            vec![ext("k", &[("c0", true)], RecordSnapshot::Unreadable, false)],
            vec![link("k", "c1")],
        );
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn plan_finish_removal_skipped_when_another_link_points_in() {
        let mut foreign = link("foo", "c0");
        foreign.target = SymlinkTarget::Checkout {
            id: "k".into(),
            commit: "c0".into(),
            origin_url: None,
        };
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true)],
                rec("k", "c0", RecordState::Removing),
                false,
            )],
            vec![foreign],
        );
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn plan_staging_kept_while_a_folderless_id_is_locked() {
        let mut s = snap(vec![], vec![]);
        s.staging = vec!["n".into()];
        s.locked_ids_without_folder = vec!["fresh".into()];
        assert!(plan(&s).is_empty());
        s.locked_ids_without_folder.clear();
        assert_eq!(plan(&s), vec![Fix::DeleteStaging { nonce: "n".into() }]);
    }

    #[test]
    fn plan_record_id_mismatch_is_left_alone() {
        let s = snap(
            vec![ext(
                "k",
                &[("c0", true)],
                rec("other", "c0", RecordState::Installed),
                false,
            )],
            vec![link("k", "c1")],
        );
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn unreadable_io_error_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        fs::create_dir(&path).unwrap();
        assert!(read(&path).is_err());
        assert!(path.exists());
        assert!(!dir.path().join("record.json.bad").exists());
    }
}

mod preflight_tests {
    use super::*;

    const GOOD: ToolProbe = ToolProbe {
        git: Probe::Found,
        cargo: Probe::Found,
        rustc_sysroot: Probe::Found,
        wasm_target: Probe::Found,
        rustup: Probe::Found,
    };

    fn facts() -> ManifestFacts {
        ManifestFacts {
            has_extension_toml: true,
            schema_version: 1,
            id: "demo".into(),
            cargo_toml: Some("[dependencies]\nzed_extension_api = \"0.7.0\"\n".into()),
            ..Default::default()
        }
    }

    fn run(probe: &ToolProbe, facts: &ManifestFacts) -> Result<(), Refusal> {
        check_after_clone(
            probe,
            facts,
            |host| host == "blocked.example",
            |id| id == "banned",
            |id| id == "other",
            1..=1,
            ApiVersion::new(0, 7, 0),
        )
    }

    fn url(text: &str) -> SourceUrl {
        SourceUrl::parse(text).unwrap()
    }

    fn message(result: Result<(), Refusal>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn preflight_passes_when_everything_is_fine() {
        assert_eq!(run(&GOOD, &facts()), Ok(()));
        assert_eq!(
            check_before_clone(&GOOD, &url("https://github.com/a/b"), |_| false),
            Ok(())
        );
        assert_eq!(ApiVersion::new(0, 7, 0).to_string(), "0.7.0");
        assert_eq!(ApiVersion::parse("0.7"), Some(ApiVersion::new(0, 7, 0)));
    }

    #[test]
    fn preflight_toolchain_messages() {
        let mut probe = GOOD;
        probe.cargo = Probe::Missing;
        assert_eq!(message(run(&probe, &facts())), NO_RUST_MESSAGE);

        let mut probe = GOOD;
        probe.wasm_target = Probe::Missing;
        assert_eq!(
            message(run(&probe, &facts())),
            "Run `rustup target add wasm32-wasip2`, then install again."
        );
        probe.rustup = Probe::Missing;
        assert_eq!(
            message(run(&probe, &facts())),
            "Add the wasm32-wasip2 target to your Rust toolchain."
        );
    }

    #[test]
    fn preflight_timed_out_probe_names_the_tool() {
        let mut probe = GOOD;
        probe.git = Probe::TimedOut;
        assert!(message(run(&probe, &facts())).starts_with("git did not respond"));
        assert!(
            message(check_before_clone(
                &probe,
                &url("https://github.com/a/b"),
                |_| false
            ))
            .contains("git did not respond")
        );
        let mut probe = GOOD;
        probe.rustc_sysroot = Probe::TimedOut;
        assert!(message(run(&probe, &facts())).starts_with("rustc did not respond"));
    }

    #[test]
    fn preflight_api_refusal_names_both_versions() {
        let expected =
            "This extension needs extension API 0.8.0; this bongos supports up to 0.7.0.";
        for cargo in [
            "[dependencies]\nzed_extension_api = \"0.8.0\"\n",
            "[dependencies]\nzed_extension_api = { version = \"^0.8\" }\n",
        ] {
            let mut f = facts();
            f.cargo_toml = Some(cargo.into());
            assert_eq!(message(run(&GOOD, &f)), expected);
        }
        let mut f = facts();
        f.cargo_toml = Some("[dependencies]\nzed_extension_api = { workspace = true }\n".into());
        f.workspace_toml =
            Some("[workspace.dependencies]\nzed_extension_api = \"~0.8.0\"\n".into());
        assert_eq!(message(run(&GOOD, &f)), expected);
        assert_eq!(
            check_built_api(ApiVersion::new(0, 8, 0), ApiVersion::new(0, 7, 0))
                .unwrap_err()
                .to_string(),
            expected
        );
        assert_eq!(api_requirement("[dependencies]\nfoo = \"1\"\n", None), None);
        assert_eq!(
            api_requirement("[dependencies]\nzed_extension_api = \">=0.6\"\n", None),
            Some(ApiVersion::new(0, 6, 0))
        );
    }

    #[test]
    fn preflight_no_cargo_toml_needs_no_rust() {
        let mut f = facts();
        f.cargo_toml = None;
        let mut probe = ToolProbe::ALL_MISSING;
        probe.git = Probe::Found;
        assert_eq!(run(&probe, &f), Ok(()));
        f.grammar_repositories = vec!["https://github.com/a/b".into()];
        assert_eq!(run(&probe, &f), Ok(()));
    }

    #[test]
    fn preflight_blocked_hosts_refused() {
        let blocked = |h: &str| h == "blocked.example";
        assert_eq!(
            message(check_before_clone(
                &GOOD,
                &url("https://blocked.example/a/b"),
                blocked
            )),
            "The repository host blocked.example is not allowed; use a repository on a different host."
        );
        for text in [
            "https://BLOCKED.example/x/y",
            "https://blocked.example./x/y",
            "git@Blocked.Example:x/y",
        ] {
            assert!(
                message(check_before_clone(&GOOD, &url(text), blocked)).contains("blocked.example"),
                "{text}"
            );
            let mut f = facts();
            f.grammar_repositories = vec![text.into()];
            assert!(
                message(run(&GOOD, &f)).contains("blocked.example"),
                "{text}"
            );
        }
        for text in ["http://github.com/a/b", "git://github.com/a/b", "https://"] {
            let mut f = facts();
            f.grammar_repositories = vec![text.into()];
            let refusal = run(&GOOD, &f).unwrap_err();
            assert_eq!(refusal, Refusal::BadGrammarUrl(text.into()));
            assert!(refusal.to_string().contains(text));
        }
    }

    #[test]
    fn preflight_git_is_checked_before_the_blocked_host() {
        let mut probe = GOOD;
        probe.git = Probe::TimedOut;
        let result = check_before_clone(&probe, &url("https://blocked.example/a"), |h| {
            h == "blocked.example"
        });
        assert_eq!(result, Err(Refusal::ToolTimedOut(Tool::Git)));
    }

    #[test]
    fn preflight_suppressed_wins_over_blocked_grammar_host() {
        let mut f = facts();
        f.id = "banned".into();
        f.grammar_repositories = vec!["https://blocked.example/x".into()];
        assert_eq!(run(&GOOD, &f), Err(Refusal::Suppressed("banned".into())));
    }

    #[test]
    fn preflight_timed_out_rustup_names_rustup() {
        let mut probe = GOOD;
        probe.wasm_target = Probe::Missing;
        probe.rustup = Probe::TimedOut;
        assert_eq!(
            run(&probe, &facts()),
            Err(Refusal::ToolTimedOut(Tool::Rustup))
        );
        assert!(message(run(&probe, &facts())).starts_with("rustup did not respond"));
    }

    #[test]
    fn preflight_lowest_allowed_handles_requirement_lists() {
        for (text, want) in [
            ("<0.8", (0, 0, 0)),
            ("<=0.8", (0, 0, 0)),
            (">0.7.0", (0, 7, 1)),
            ("<0.9, >=0.8", (0, 8, 0)),
            (">=0.6, <0.8", (0, 6, 0)),
            ("^0.7", (0, 7, 0)),
            ("~0.7.1", (0, 7, 1)),
            ("=0.7.2", (0, 7, 2)),
            ("0.7", (0, 7, 0)),
        ] {
            assert_eq!(
                ApiVersion::lowest_allowed(text),
                Some(ApiVersion::new(want.0, want.1, want.2)),
                "{text}"
            );
        }
    }

    #[test]
    fn preflight_old_manifest_refused() {
        let mut f = facts();
        f.has_extension_toml = false;
        f.has_extension_json = true;
        let text = message(run(&GOOD, &f));
        assert!(text.contains("extension.json") && text.contains("extension.toml"));
        f.has_extension_json = false;
        assert_eq!(message(run(&GOOD, &f)), MANIFEST_MISSING_MESSAGE);
    }

    #[test]
    fn preflight_suppressed_and_installed_elsewhere_refused() {
        let mut f = facts();
        f.id = "banned".into();
        assert!(message(run(&GOOD, &f)).contains("banned"));
        f.id = "other".into();
        assert_eq!(
            message(run(&GOOD, &f)),
            "other is already installed from another source; remove other first."
        );
    }

    #[test]
    fn preflight_empty_path_gives_exact_no_rust_message() {
        let mut probe = ToolProbe::ALL_MISSING;
        probe.git = Probe::Found;
        assert_eq!(message(run(&probe, &facts())), NO_RUST_MESSAGE);
        assert_eq!(
            NO_RUST_MESSAGE,
            "Building this extension needs Rust: install rustup from https://rustup.rs, then run `rustup target add wasm32-wasip2`. If Rust is installed, start bongos from a shell or add `~/.cargo/bin` to the desktop session's PATH."
        );
    }
}
