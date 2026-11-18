//! Output retention. At start and every 24 hours, the output folder of each finished check run
//! older than `task_checks.output_retention_days` is deleted and its evidence record is marked
//! `output = "deleted"`. The record stays, and so do the done record and the verdict. Only the
//! record's own folder under the task's output folder (or the landing output folder) is ever
//! removed, and a symlink is removed as a link, never followed.

use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bongos_checks_model::evidence::{EvidenceRecord, Output, StartedBy};
use bongos_project_repo::StateWriter;
use chrono::{DateTime, Utc};
use futures::FutureExt as _;
use gpui::{Context, Task};
use settings::Settings as _;

use crate::engine::ChecksEngine;
use crate::evidence_store::{EVIDENCE_NAMESPACE, LANDING_PREFIX, record_key, store_record};
use crate::settings::TaskChecksSettings;

/// How long the sweep waits between two runs.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

fn is_plain_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Whether `record` is finished, still has output, and ended `days` or more before `now`.
fn expired(record: &EvidenceRecord, now: DateTime<Utc>, days: u32) -> bool {
    if !record.is_finished() || !matches!(record.output, Output::Path(_)) {
        return false;
    }
    let Some(ended) = record
        .ended
        .as_deref()
        .or(Some(record.started.as_str()))
        .and_then(parse_time)
    else {
        return false;
    };
    now.signed_duration_since(ended) >= chrono::Duration::days(i64::from(days))
}

/// Removes the output folder of `record` when it is exactly `<output folder>/<record id>`.
/// Returns whether the output is gone (a folder that was already missing counts).
fn remove_output(settings: &TaskChecksSettings, files: &Path, record: &EvidenceRecord) -> bool {
    let Output::Path(recorded) = &record.output else {
        return false;
    };
    let scope = match &record.started_by {
        StartedBy::Landing { .. } => LANDING_PREFIX,
        _ => record.task_id.as_str(),
    };
    if !is_plain_component(scope) || !is_plain_component(&record.id) {
        return false;
    }
    let root = settings.output_folder(files, scope);
    let folder = root.join(&record.id);
    if Path::new(recorded) != folder {
        return false;
    }
    // A root that is a link could lead anywhere.
    match std::fs::symlink_metadata(&root) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => return false,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    }
    let result = match std::fs::symlink_metadata(&folder) {
        Ok(meta) if meta.file_type().is_dir() => std::fs::remove_dir_all(&folder),
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(&folder),
        Ok(_) => return false,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            log::warn!("checks: deleting the output {}: {error}", folder.display());
            false
        }
    }
}

/// One sweep: deletes the output of every record that ended `output_retention_days` or more
/// before `now` (RFC 3339 text) and marks the record deleted. Returns how many records it marked.
pub(crate) fn sweep_output(
    writer: &StateWriter,
    settings: &TaskChecksSettings,
    files: &Path,
    now: &str,
) -> Result<usize> {
    let Some(now) = parse_time(now) else {
        return Ok(0);
    };
    writer.transact(|tx| {
        let rows = tx.kv(EVIDENCE_NAMESPACE).scan("")?;
        let mut marked = 0;
        for (key, value) in rows {
            let Ok(mut record) = value.try_into::<EvidenceRecord>() else {
                continue;
            };
            if !expired(&record, now, settings.output_retention_days)
                || record_key(&record) != key
                || !remove_output(settings, files, &record)
            {
                continue;
            }
            record.output = Output::Deleted;
            store_record(tx, &record)?;
            marked += 1;
        }
        Ok(marked)
    })
}

impl ChecksEngine {
    /// Sweeps the output of the project whose writer is `writer` now and every 24 hours on the
    /// engine's clock, while `task_checks.enabled` is true. A clock that never waits ends the
    /// loop after its first sweep.
    pub(crate) fn start_retention(
        &mut self,
        writer: &Arc<StateWriter>,
        files: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let weak = Arc::downgrade(writer);
        cx.spawn(async move |this, cx| {
            loop {
                let Some(writer) = weak.upgrade() else {
                    break;
                };
                let Ok((settings, now, clock)) = this.update(cx, |engine, cx| {
                    (
                        TaskChecksSettings::get_global(cx).clone(),
                        (engine.now)(),
                        engine.clock.clone(),
                    )
                }) else {
                    break;
                };
                if settings.enabled {
                    let files = files.clone();
                    let swept = cx
                        .background_executor()
                        .spawn(async move { sweep_output(&writer, &settings, &files, &now) })
                        .await;
                    if let Err(error) = swept {
                        log::warn!("checks: sweeping old output: {error:#}");
                    }
                } else {
                    drop(writer);
                }
                let mut sleep = clock.sleep(SWEEP_INTERVAL);
                if (&mut sleep).now_or_never().is_some() {
                    break;
                }
                sleep.await;
            }
        })
    }
}
