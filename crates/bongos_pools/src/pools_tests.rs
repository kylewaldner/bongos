use std::path::Path;
use std::time::Duration;

use bongos_lanes::{LaneTableFields, validate_folder_accepting};
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::{Settings as _, SettingsStore};

use crate::{
    HooksSettings, LANE_TABLE_KEYS, PoolsSettings, QueuesSettings, TriggersSettings, is_registered,
    validate_pools_folder, validate_queues_folder, validate_triggers_folder,
    workflow_reference_diagnostics,
};

fn use_default_settings(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        crate::init(cx);
    });
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.pools.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
fn settings_defaults_match_the_design(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| {
        let pools = PoolsSettings::get_global(cx);
        assert_eq!(
            *pools,
            PoolsSettings {
                enabled: false,
                machine_store_dir: None,
                poll_interval: Duration::from_millis(1000),
                lock_timeout: Duration::from_secs(10),
                mixed_head: Duration::from_secs(60),
                guard_interval: Duration::from_secs(5),
                guard_settle: Duration::from_secs(5),
                guard_script_timeout: Duration::from_secs(30),
                max_hold: None,
            }
        );
        assert_eq!(
            *QueuesSettings::get_global(cx),
            QueuesSettings {
                max_attempts: 3,
                failures_kept: 10,
            }
        );
        assert_eq!(
            *TriggersSettings::get_global(cx),
            TriggersSettings {
                keep_host_alive: false,
                scheduled_tolerance: Duration::from_secs(30 * 60),
                history_limit: 50,
                preflight_timeout: Duration::from_secs(120),
            }
        );
        assert_eq!(
            *HooksSettings::get_global(cx),
            HooksSettings {
                timeout: Duration::from_secs(900),
                output_tail_bytes: 16 * 1024,
            }
        );
    });
}

const BAD_POOLS: &str = "schema = 1\n[[pool]]\nname = \"jobs\"\nkind = \"slots\"\n";
const PULLS_TABLE: &str = "schema = 1\n\n[[kind]]\nname = \"build\"\npulls = [{ queue = \"fixes\" }]\n[kind.binding]\npools = [\"jobs\"]\n";

#[gpui::test]
fn nothing_is_registered_or_judged_while_disabled(cx: &mut TestAppContext) {
    use_default_settings(cx);
    cx.update(|cx| assert!(!is_registered(cx)));
    assert!(LaneTableFields::registered().is_empty());
    assert!(validate_pools_folder(Path::new("pools/pools.toml"), BAD_POOLS).is_empty());
    let refused = bongos_lanes::validate_folder(Path::new("lanes/lanes.toml"), PULLS_TABLE);
    assert_eq!(refused.len(), 2, "{refused:?}");
}

#[gpui::test]
fn enabling_registers_the_folders_and_the_lane_table_keys(cx: &mut TestAppContext) {
    use_default_settings(cx);
    set_enabled(cx, true);
    cx.update(|cx| assert!(is_registered(cx)));
    let mut registered = LaneTableFields::registered();
    registered.sort();
    let mut expected = LANE_TABLE_KEYS.map(String::from).to_vec();
    expected.sort();
    assert_eq!(registered, expected);
    assert!(bongos_lanes::validate_folder(Path::new("lanes/lanes.toml"), PULLS_TABLE).is_empty());

    let problems = validate_pools_folder(Path::new("pools/pools.toml"), BAD_POOLS);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, "pools/pools.toml");
    assert_eq!(
        problems[0].reason,
        "line 2: slots pool `jobs` needs a size of at least 1"
    );
    assert!(validate_pools_folder(Path::new("pools/scripts/enough-swap"), "#!/bin/sh").is_empty());
    assert_eq!(
        validate_queues_folder(
            Path::new("queues/queues.toml"),
            "schema = 1\n[[queue]]\nname = \"q\"\nitem = { t = { type = \"text\" } }\ntask_field = \"t\"\n"
        )
        .len(),
        1
    );
    assert_eq!(
        validate_triggers_folder(
            Path::new("triggers/triggers.toml"),
            "schema = 1\n[[trigger]]\nname = \"t\"\non = { task_done = \"x\" }\naction = { workflow = \"w\", base = \"main\" }\n"
        )
        .len(),
        1
    );

    set_enabled(cx, false);
    assert!(LaneTableFields::registered().is_empty());
    assert!(validate_pools_folder(Path::new("pools/pools.toml"), BAD_POOLS).is_empty());
}

#[test]
fn the_lane_table_seam_accepts_only_registered_keys() {
    let path = Path::new("lanes/lanes.toml");
    let refused = validate_folder_accepting(path, PULLS_TABLE, &[]);
    let reasons: Vec<&str> = refused
        .iter()
        .map(|problem| problem.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "line 5: pulls are not available yet",
            "line 7: unknown key \"pools\" in [kind.binding]",
        ]
    );
    let only_pulls = validate_folder_accepting(path, PULLS_TABLE, &["kind.pulls"]);
    let reasons: Vec<&str> = only_pulls
        .iter()
        .map(|problem| problem.reason.as_str())
        .collect();
    assert_eq!(reasons, ["line 7: unknown key \"pools\" in [kind.binding]"]);
    assert!(validate_folder_accepting(path, PULLS_TABLE, &LANE_TABLE_KEYS).is_empty());
}

#[test]
fn the_validation_hook_reports_problems_in_the_workflows_own_file() {
    let root = tempfile::tempdir().unwrap();
    let definitions = root.path().join("definitions");
    let dir = definitions.join("workflows").join("build");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(definitions.join("project.toml"), "").unwrap();
    let text = "schema = 1\nname = \"build\"\nstart = \"work\"\n[nodes.work]\nkind = \"agent\"\nprompt = \"work.md\"\nneeds = [\"jobs\"]\n";
    std::fs::write(dir.join("workflow.toml"), text).unwrap();
    let definition = bongos_workflow_model::parse(text).unwrap();

    crate::ENABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let diagnostics = workflow_reference_diagnostics(&definition, &dir);
    let messages: Vec<(usize, &str)> = diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.message.as_str()))
        .collect();
    assert_eq!(
        messages,
        [(7, "pool `jobs` is not declared in pools/pools.toml")]
    );

    std::fs::create_dir_all(definitions.join("pools")).unwrap();
    std::fs::write(
        definitions.join("pools/pools.toml"),
        "schema = 1\n[[pool]]\nname = \"jobs\"\nkind = \"rw\"\n",
    )
    .unwrap();
    assert!(workflow_reference_diagnostics(&definition, &dir).is_empty());

    crate::ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    std::fs::remove_file(definitions.join("pools/pools.toml")).unwrap();
    assert!(workflow_reference_diagnostics(&definition, &dir).is_empty());
}
