//! Tests of the Checks section source, the output view and `task: edit checks`: what they show,
//! where an edit opens, and that an edit never reaches a running check's pin. No test starts an
//! agent or calls a model, and none depends on the calendar.

use std::path::Path;
use std::sync::Arc;

use bongos_checks_model::evidence::{EvidenceRecord, Output};
use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
use bongos_work::{SectionTone, TaskSectionsRegistry};
use futures::executor::block_on;
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::SettingsStore;
use tempfile::TempDir;

use crate::edit::{checks_table_row, edit_target};
use crate::engine_tests::World;
use crate::output_view::{DELETED_MESSAGE, latest_per_check, read_output, view_text};
use crate::pin::{Pin, master_definitions};
use crate::section_source::{convert, summary_line};
use crate::task_source::effective_set_in;
use crate::test_support::{
    definition_toml, fixture_project, land, new_copy, person, record, write_definition,
};

const TASK_FILE: &str = "tasks/backfill/task.toml";

fn checks(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("\n[[checks.check]]\nname = \"{name}\"\ncommand = [\"true\"]\n"))
        .collect()
}

fn names_in(definitions: &Path) -> Vec<String> {
    let set = effective_set_in(definitions, "l1", &|_| false).expect("the definitions parse");
    set.rows.iter().map(|row| row.decl.name.clone()).collect()
}

#[test]
fn the_edit_opens_the_task_file_or_the_named_set_at_the_checks_table() {
    assert_eq!(
        edit_target(Some(Path::new("tasks/backfill")), None),
        Path::new("tasks/backfill/task.toml")
    );
    assert_eq!(edit_target(None, None), Path::new("task.toml"));
    assert_eq!(
        edit_target(Some(Path::new("tasks/backfill")), Some("rust")),
        Path::new("checks/sets/rust.toml")
    );

    let with_table = "schema = 1\nid = \"l1\"\n\n[checks]\ninherit = \"none\"\n";
    assert_eq!(checks_table_row(with_table), 3);
    let with_array = "schema = 1\n\n[[checks.check]]\nname = \"a\"\n";
    assert_eq!(checks_table_row(with_array), 2);
    assert_eq!(checks_table_row("[checks.sets]\n"), 0);
    // No table, or a table that only starts with the word: the top of the file.
    assert_eq!(checks_table_row("schema = 1\n[checkset]\n"), 0);
    assert_eq!(checks_table_row(""), 0);
}

#[test]
fn an_edit_in_a_working_copy_never_reaches_a_pin_that_is_held() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let setup = new_copy(&repo, "setup", &person());
    write_definition(
        &setup,
        TASK_FILE,
        &definition_toml("l1", "Backfill", &checks(&["tests"])),
    );
    let landed = land(&repo, &setup, &person());
    assert!(
        matches!(landed, bongos_project_repo::LandingResult::Landed { .. }),
        "{landed:?}"
    );
    // A running check holds the pin it started with.
    let pin = Arc::new(block_on(Pin::materialize(&repo, "main")).unwrap());
    assert_eq!(names_in(&pin.definitions()), ["tests"]);

    // The person edits the checks in a working copy: the copy changes, nothing else.
    let copy = new_copy(&repo, "edit-checks", &person());
    let before = std::fs::read_to_string(pin.definitions().join(TASK_FILE)).unwrap();
    write_definition(
        &copy,
        TASK_FILE,
        &definition_toml("l1", "Backfill", &checks(&["tests", "lint"])),
    );
    assert_eq!(names_in(&copy.path.join("definitions")), ["tests", "lint"]);
    assert_eq!(names_in(&pin.definitions()), ["tests"]);
    assert_eq!(names_in(&master_definitions(&repo)), ["tests"]);

    // Landing changes the set the next run reads; the pin of the running check stays.
    let landed = land(&repo, &copy, &person());
    assert!(
        matches!(landed, bongos_project_repo::LandingResult::Landed { .. }),
        "{landed:?}"
    );
    assert_eq!(names_in(&master_definitions(&repo)), ["tests", "lint"]);
    assert_eq!(names_in(&pin.definitions()), ["tests"]);
    assert_eq!(
        std::fs::read_to_string(pin.definitions().join(TASK_FILE)).unwrap(),
        before
    );
}

fn finished(check: &str, id: &str, verdict: Verdict, output: Output) -> EvidenceRecord {
    let mut record = record("l1", "tree1", check, id);
    record.verdict = verdict;
    record.output = output;
    record
}

#[test]
fn the_output_view_has_the_record_as_a_header_then_the_parts() {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("out");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("part-2.log"), "second").unwrap();
    std::fs::write(folder.join("part-1.log"), "first\n").unwrap();
    std::fs::write(folder.join("notes.txt"), "not a part").unwrap();
    let mut rec = finished(
        "tests",
        "2",
        Verdict::Fail,
        Output::Path(folder.to_string_lossy().into_owned()),
    );
    rec.failing = vec!["t::a".into()];
    let text = view_text(&rec, read_output(&rec).as_deref());
    assert!(
        text.starts_with("check: tests\ntask: l1\nresult: fail\n"),
        "{text}"
    );
    assert!(text.contains("failing: t::a"), "{text}");
    assert!(
        text.ends_with("--- part 1 ---\nfirst\n--- part 2 ---\nsecond\n"),
        "{text}"
    );
}

#[test]
fn a_record_whose_output_is_gone_says_so() {
    let gone = finished("tests", "3", Verdict::Pass, Output::Deleted);
    assert_eq!(read_output(&gone), None);
    assert!(view_text(&gone, None).ends_with(DELETED_MESSAGE));
    let missing = finished(
        "tests",
        "4",
        Verdict::Pass,
        Output::Path("/no/such/folder".into()),
    );
    assert!(view_text(&missing, read_output(&missing).as_deref()).ends_with(DELETED_MESSAGE));
    // A part that has not written yet is not a deleted output.
    let mut running = finished(
        "tests",
        "5",
        Verdict::NoVerdict(NoVerdictReason::Interrupted),
        Output::Path("/no/such/folder".into()),
    );
    running.ended = None;
    let text = view_text(&running, None);
    assert!(text.contains("result: running"), "{text}");
    assert!(!text.contains(DELETED_MESSAGE), "{text}");
}

#[test]
fn the_latest_record_of_each_check_is_the_one_shown() {
    let records = [
        finished("lint", "1", Verdict::Fail, Output::Deleted),
        finished("tests", "2", Verdict::Pass, Output::Deleted),
        finished("lint", "3", Verdict::Pass, Output::Deleted),
    ];
    let latest: Vec<(&str, &str)> = latest_per_check(&records)
        .into_iter()
        .map(|record| (record.check.as_str(), record.id.as_str()))
        .collect();
    assert_eq!(latest, [("lint", "3"), ("tests", "2")]);
}

#[test]
fn the_section_converts_with_its_tones_and_the_summary_names_the_header() {
    use bongos_checks_model::{SectionRow, TaskSection, Tone};
    let row = |id: &str, tone, detail: &[&str]| SectionRow {
        id: id.to_string(),
        text: format!("{id} text"),
        tone,
        detail: detail.iter().map(|line| line.to_string()).collect(),
    };
    let section = TaskSection {
        title: "Checks".into(),
        header: "not done: 2 checks missing".into(),
        rows: vec![
            row("a", Tone::Success, &[]),
            row("b", Tone::Error, &["one", "two"]),
            row("c", Tone::Warning, &[]),
            row("d", Tone::Muted, &[]),
            row("e", Tone::Normal, &[]),
        ],
    };
    let converted = convert(section, vec![row("source/rows/a", Tone::Muted, &[])]);
    assert_eq!(converted.title, "Checks");
    let tones: Vec<SectionTone> = converted.rows.iter().map(|row| row.tone).collect();
    assert_eq!(
        tones,
        [
            SectionTone::Success,
            SectionTone::Error,
            SectionTone::Warning,
            SectionTone::Muted,
            SectionTone::Neutral,
            SectionTone::Muted,
        ]
    );
    assert_eq!(converted.rows[0].detail, None);
    assert_eq!(converted.rows[1].detail.as_deref(), Some("one\ntwo"));
    assert_eq!(
        summary_line(&converted.header),
        "Checks: not done: 2 checks missing"
    );
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_checks.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn the_source_gives_the_section_and_the_summary_through_the_registry(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);
    cx.update(|cx| crate::section_source::init(&world.engine, cx));

    let (sections, summaries) = cx.update(|cx| {
        (
            TaskSectionsRegistry::sections("l1", false, cx),
            TaskSectionsRegistry::summaries("l1", cx),
        )
    });
    assert_eq!(sections.len(), 1);
    let (name, section) = &sections[0];
    assert_eq!(name, "checks");
    assert_eq!(section.title, "Checks");
    assert_eq!(section.header, "not done: 4 checks missing");
    assert_eq!(section.rows.len(), 4);
    assert!(
        section
            .rows
            .iter()
            .all(|row| row.tone == SectionTone::Muted)
    );
    assert_eq!(summaries, ["Checks: not done: 4 checks missing"]);

    // With the sources shown, the derivation rows follow the check rows.
    let with_sources = cx.update(|cx| TaskSectionsRegistry::sections("l1", true, cx));
    let rows = &with_sources[0].1.rows;
    assert!(rows.len() > 4);
    assert!(rows[4..].iter().all(|row| row.id.starts_with("source/")));
    assert!(rows[4].text.contains("uses: own"), "{}", rows[4].text);

    // A run shows in the row of the check it ran.
    let report = world.run(cx, world.only(&["tests"])).await;
    assert!(!report.results().is_empty());
    let after = cx.update(|cx| TaskSectionsRegistry::sections("l1", false, cx));
    let tests = after[0]
        .1
        .rows
        .iter()
        .find(|row| row.id == "tests")
        .unwrap();
    assert_eq!(tests.tone, SectionTone::Success);
    assert!(tests.text.contains("| pass"), "{}", tests.text);

    // Nothing shows while the setting is off.
    set_enabled(cx, false);
    cx.update(|cx| {
        assert!(TaskSectionsRegistry::sections("l1", true, cx).is_empty());
        assert!(TaskSectionsRegistry::summaries("l1", cx).is_empty());
    });
    set_enabled(cx, true);
    cx.update(|cx| assert_eq!(TaskSectionsRegistry::sections("l1", false, cx).len(), 1));
}
