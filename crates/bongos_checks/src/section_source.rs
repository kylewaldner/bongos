//! The Checks section of a task's details tab and its line in the Work panel. Both come from the
//! pure section the engine builds; this converts it to the details tab's types. Nothing shows
//! while `task_checks.enabled` is false.

use std::rc::Rc;

use bongos_checks_model::{Tone, source_rows};
use bongos_work::{SectionRow, SectionTone, TaskSection, TaskSectionSource, TaskSectionsRegistry};
use gpui::{App, Window};
use settings::{Settings as _, SettingsStore};

use crate::actions::open_output_of;
use crate::engine::{ChecksEngine, ChecksEvent};
use crate::settings::TaskChecksSettings;

/// The name of the source in the registry.
pub(crate) const SOURCE_NAME: &str = "checks";

/// Where the Checks section sorts among the sections of the details tab: first.
const ORDER: i32 = i32::MIN;

fn tone(tone: Tone) -> SectionTone {
    match tone {
        Tone::Normal => SectionTone::Neutral,
        Tone::Success => SectionTone::Success,
        Tone::Error => SectionTone::Error,
        Tone::Warning => SectionTone::Warning,
        Tone::Muted => SectionTone::Muted,
    }
}

fn row(row: bongos_checks_model::SectionRow) -> SectionRow {
    SectionRow {
        id: row.id,
        text: row.text,
        tone: tone(row.tone),
        detail: (!row.detail.is_empty()).then(|| row.detail.join("\n")),
    }
}

/// The section of the details tab for the model's `section`, with the derivation rows of the
/// effective set after the check rows when `sources` has them.
pub(crate) fn convert(
    section: bongos_checks_model::TaskSection,
    sources: Vec<bongos_checks_model::SectionRow>,
) -> TaskSection {
    TaskSection {
        title: section.title,
        header: section.header,
        rows: section.rows.into_iter().chain(sources).map(row).collect(),
    }
}

/// The line of the expanded row of a task in the Work panel.
pub(crate) fn summary_line(header: &str) -> String {
    format!("Checks: {header}")
}

struct ChecksSectionSource;

impl TaskSectionSource for ChecksSectionSource {
    fn section(&self, task_id: &str, show_sources: bool, cx: &App) -> Option<TaskSection> {
        if !TaskChecksSettings::get_global(cx).enabled {
            return None;
        }
        let engine = ChecksEngine::global(cx)?;
        let engine = engine.read(cx);
        let status = engine.status(task_id, cx)?;
        let sources = match (show_sources, engine.task(task_id, cx)) {
            (true, Some(task)) => source_rows(&task.set),
            _ => Vec::new(),
        };
        Some(convert(status.section, sources))
    }

    fn summary(&self, task_id: &str, cx: &App) -> Option<String> {
        if !TaskChecksSettings::get_global(cx).enabled {
            return None;
        }
        let status = ChecksEngine::global(cx)?.read(cx).status(task_id, cx)?;
        Some(summary_line(&status.section.header))
    }

    fn activate(&self, task_id: &str, row_id: &str, window: &mut Window, cx: &mut App) {
        // The rows of the sources view and of dropped checks name no check to open.
        if row_id.starts_with("source/") || row_id.starts_with("dropped/") {
            return;
        }
        let (task_id, check) = (task_id.to_string(), row_id.to_string());
        if let Some(workspace) = window.root::<workspace::MultiWorkspace>().flatten() {
            workspace
                .read(cx)
                .workspace()
                .clone()
                .update(cx, |workspace, cx| {
                    open_output_of(&task_id, &check, workspace, window, cx)
                });
        }
    }
}

/// Registers the source, tells the details tabs when the checks change, and again when the
/// setting does. The source answers nothing while `task_checks.enabled` is false.
pub(crate) fn init(engine: &gpui::Entity<ChecksEngine>, cx: &mut App) {
    TaskSectionsRegistry::add_source(SOURCE_NAME, ORDER, Rc::new(ChecksSectionSource), cx);
    // An event can come from inside an update of the Work store, which the notification
    // touches too, so it goes out after the current update.
    cx.subscribe(engine, |_, _: &ChecksEvent, cx| {
        cx.defer(TaskSectionsRegistry::changed);
    })
    .detach();
    let mut enabled = TaskChecksSettings::try_get(cx).is_some_and(|settings| settings.enabled);
    cx.observe_global::<SettingsStore>(move |cx| {
        let now = TaskChecksSettings::get_global(cx).enabled;
        if now != enabled {
            enabled = now;
            cx.defer(TaskSectionsRegistry::changed);
        }
    })
    .detach();
}
