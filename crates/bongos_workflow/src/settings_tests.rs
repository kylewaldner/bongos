//! Tests of the `workflows` settings: the shipped defaults, the user overrides and the validation
//! context they give.

use bongos_workflow_model::Ceilings;
use bongos_workflow_model::unsandboxed::Mode;
use gpui::{App, TestAppContext};
use settings::{Settings, SettingsStore};
use std::path::PathBuf;

use crate::WorkflowsSettings;

/// The settings read from the shipped defaults under `user` (the text of a user settings file).
fn read(user: &str, cx: &mut App) -> WorkflowsSettings {
    release_channel::init(semver::Version::new(0, 0, 0), cx);
    let mut store = SettingsStore::new(cx, &settings::default_settings());
    store.set_user_settings(user, cx).unwrap();
    WorkflowsSettings::from_settings(store.merged_settings())
}

fn workflows(body: &str) -> String {
    format!(r#"{{ "workflows": {{ {body} }} }}"#)
}

#[gpui::test]
fn defaults_are_the_documented_values(cx: &mut TestAppContext) {
    let settings = cx.update(|cx| read("{}", cx));
    assert!(settings.enabled);
    assert_eq!(settings.limits.run_max_cost_usd, None);
    assert_eq!(settings.limits.loop_bound, 5);
    assert_eq!(settings.definitions_dir, None);
    assert!(settings.mcp_exclude.is_empty());
    assert_eq!(settings.agents.max_turns, 40);
    assert_eq!(settings.agents.max_cost_usd, 5.0);
    assert_eq!(settings.agents.max_minutes, 30);
    assert_eq!(settings.agents.model, "sonnet");
    assert!(settings.programs.allow_network);
    assert_eq!(settings.programs.unsandboxed, Mode::Fallback);
    assert_eq!(settings.ceilings, Ceilings::default());
    assert_eq!(settings, WorkflowsSettings::default());
}

#[gpui::test]
fn run_cost_cap_reads_a_positive_number_and_nothing_else(cx: &mut TestAppContext) {
    let capped = cx.update(|cx| read(&workflows(r#""run_max_cost_usd": 5.0"#), cx));
    assert_eq!(capped.limits.run_max_cost_usd, Some(5.0));
    let zero = cx.update(|cx| read(&workflows(r#""run_max_cost_usd": 0"#), cx));
    assert_eq!(zero.limits.run_max_cost_usd, None);
    let negative = cx.update(|cx| read(&workflows(r#""run_max_cost_usd": -3"#), cx));
    assert_eq!(negative.limits.run_max_cost_usd, None);
    let null = cx.update(|cx| read(&workflows(r#""run_max_cost_usd": null"#), cx));
    assert_eq!(null.limits.run_max_cost_usd, None);
}

#[gpui::test]
fn a_user_value_overrides_each_key(cx: &mut TestAppContext) {
    let user = workflows(
        r#"
        "loop_bound": 9,
        "definitions_dir": "/srv/flows",
        "mcp_exclude": ["slack", "jira"],
        "agents": {
            "max_turns": 10,
            "max_cost_usd": 1.5,
            "max_minutes": 7,
            "claude": { "model": "opus" }
        },
        "ceilings": {
            "max_turns": 11,
            "max_cost_usd": 12.5,
            "max_minutes": 13,
            "max_passes": 14,
            "retries": 15,
            "program_max_minutes": 16,
            "allow_bypass_permissions": true
        },
        "programs": { "allow_network": false }
        "#,
    );
    let settings = cx.update(|cx| read(&user, cx));
    assert_eq!(settings.limits.loop_bound, 9);
    assert_eq!(settings.definitions_dir, Some(PathBuf::from("/srv/flows")));
    assert_eq!(settings.mcp_exclude, ["slack", "jira"]);
    assert_eq!(settings.agents.max_turns, 10);
    assert_eq!(settings.agents.max_cost_usd, 1.5);
    assert_eq!(settings.agents.max_minutes, 7);
    assert_eq!(settings.agents.model, "opus");
    assert!(!settings.programs.allow_network);
    assert_eq!(
        settings.ceilings,
        Ceilings {
            max_turns: 11,
            max_cost_usd: 12.5,
            max_minutes: 13,
            max_passes: 14,
            retries: 15,
            program_max_minutes: 16,
            allow_bypass_permissions: true,
        }
    );
}

#[gpui::test]
fn a_partial_user_value_keeps_the_other_defaults(cx: &mut TestAppContext) {
    let settings = cx.update(|cx| read(&workflows(r#""ceilings": { "max_turns": 50 }"#), cx));
    assert_eq!(settings.ceilings.max_turns, 50);
    assert_eq!(settings.ceilings.max_cost_usd, 25.0);
    assert_eq!(settings.ceilings.retries, 5);
}

#[gpui::test]
fn the_validation_context_carries_the_ceilings_and_allow_network(cx: &mut TestAppContext) {
    let user = workflows(
        r#""fan_out_width": 4,
        "ceilings": { "max_passes": 3 },
        "programs": { "allow_network": false }"#,
    );
    let settings = cx.update(|cx| read(&user, cx));
    let context = settings.validation_context();
    assert_eq!(context.ceilings.max_passes, 3);
    assert_eq!(context.ceilings, settings.ceilings);
    assert!(!context.allow_network);
    assert_eq!(context.fan_out_width, 4);

    let default = WorkflowsSettings::default().validation_context();
    assert_eq!(default.ceilings, Ceilings::default());
    assert!(default.allow_network);
}

#[gpui::test]
fn an_unknown_unsandboxed_value_reads_as_ask_and_the_rest_still_loads(cx: &mut TestAppContext) {
    let user = workflows(
        r#""loop_bound": 9,
        "programs": { "unsandboxed": "sometimes", "allow_network": false, "max_minutes": 3 }"#,
    );
    let settings = cx.update(|cx| read(&user, cx));
    assert_eq!(settings.programs.unsandboxed, Mode::Ask);
    assert!(!settings.programs.allow_network);
    assert_eq!(settings.programs.max_minutes, 3);
    assert_eq!(settings.limits.loop_bound, 9);

    for (value, mode) in [
        ("fallback", Mode::Fallback),
        ("ask", Mode::Ask),
        ("refuse", Mode::Refuse),
        ("allow", Mode::Allow),
    ] {
        let user = workflows(&format!(r#""programs": {{ "unsandboxed": "{value}" }}"#));
        let settings = cx.update(|cx| read(&user, cx));
        assert_eq!(settings.programs.unsandboxed, mode, "{value}");
    }
}
