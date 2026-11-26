//! The guard readers of the broker: free memory, free disk and a guard script, and which guards a
//! transaction reads afresh.
//!
//! A guard is read again only when its stored reading is older than `pools.guard_interval_seconds`
//! (or the last grant spent it and the settle time has passed), so every broker sharing a machine
//! pool applies the one reading stored in `machine.toml`. A script runs from the pinned copy of
//! the definitions with `pools.guard_script_timeout_seconds`; its exit code is the reading (0
//! passes) and its first output line is shown beside it. A script that times out or cannot start
//! fails the guard with that reason.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bongos_pools_model::{GuardCheck, GuardReadings, GuardValue, HostRef, PoolTable, Timestamp};
use futures::FutureExt as _;
use futures::future::{BoxFuture, Either};
use gpui::BackgroundExecutor;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A guard script to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRun {
    /// As the declaration writes it: a file of the definitions, or a shell command.
    pub script: String,
    /// The pinned copy of the definitions it runs in, when there is one.
    pub dir: Option<PathBuf>,
    pub timeout: Duration,
}

/// What a guard script printed and how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptOutput {
    pub exit: i32,
    pub first_line: String,
}

/// Reads the figures guards compare. Injected so tests decide every reading.
pub trait GuardReaders: Send + Sync + 'static {
    /// Available memory in bytes.
    fn free_memory_bytes(&self) -> Option<u64>;
    /// Free bytes of the disk holding `path`.
    fn free_disk_bytes(&self, path: &Path) -> Option<u64>;
    /// Runs a guard script; an error is why it gave no reading (a timeout, a failed start).
    fn run_script(
        &self,
        run: ScriptRun,
        executor: BackgroundExecutor,
    ) -> BoxFuture<'static, Result<ScriptOutput, String>>;
}

/// The readers of the real system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGuardReaders;

impl GuardReaders for SystemGuardReaders {
    fn free_memory_bytes(&self) -> Option<u64> {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        Some(system.available_memory())
    }

    fn free_disk_bytes(&self, path: &Path) -> Option<u64> {
        bongos_workflow::worktrees::disk_free_bytes(path)
    }

    fn run_script(
        &self,
        run: ScriptRun,
        executor: BackgroundExecutor,
    ) -> BoxFuture<'static, Result<ScriptOutput, String>> {
        async move {
            let mut command = smol::process::Command::new("sh");
            match &run.dir {
                Some(dir) if dir.join(&run.script).is_file() => {
                    command.arg(dir.join(&run.script));
                }
                _ => {
                    command.arg("-c").arg(&run.script);
                }
            }
            if let Some(dir) = &run.dir {
                command.current_dir(dir);
            }
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let output = command.output();
            let timer = executor.timer(run.timeout);
            match futures::future::select(Box::pin(output), timer).await {
                Either::Left((Ok(output), _)) => Ok(ScriptOutput {
                    exit: output.status.code().unwrap_or(-1),
                    first_line: String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                }),
                Either::Left((Err(error), _)) => Err(format!(
                    "guard script {} could not start: {error}",
                    run.script
                )),
                Either::Right(_) => Err(format!(
                    "guard script {} timed out after {} s",
                    run.script,
                    run.timeout.as_secs()
                )),
            }
        }
        .boxed()
    }
}

/// Where guards read from and how often.
#[derive(Clone)]
pub(crate) struct GuardInputs {
    pub readers: Arc<dyn GuardReaders>,
    pub definitions: Option<PathBuf>,
    /// The folder a disk guard without a path, or with a relative one, reads.
    pub disk_root: PathBuf,
    pub interval: Duration,
    pub script_timeout: Duration,
}

/// Fresh readings, and the text shown beside each (a script's first line, why a reading failed).
#[derive(Debug, Default)]
pub(crate) struct Fresh {
    pub readings: GuardReadings,
    pub notes: BTreeMap<String, String>,
}

/// The guarded pools of `tables` whose reading is due: a pool `host` waits in, or one of
/// `extra`, whose stored reading is missing, older than the interval, or spent by the last grant
/// with the settle time passed.
pub(crate) fn due_guards(
    tables: &[&PoolTable],
    host: &HostRef,
    extra: &BTreeSet<String>,
    now: Timestamp,
    interval: Duration,
) -> Vec<(String, GuardCheck)> {
    let interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
    let mut due = Vec::new();
    for table in tables {
        for (name, pool) in &table.pools {
            let Some(guard) = &pool.guard else {
                continue;
            };
            let wanted = extra.contains(name)
                || pool
                    .waiters
                    .iter()
                    .any(|waiter| waiter.holder.host == *host);
            if !wanted {
                continue;
            }
            let stale = match guard.reading {
                None => true,
                Some(reading) => {
                    let old = now.0.saturating_sub(reading.at.0) >= interval_ms;
                    let spent = guard.spent.is_some_and(|spent| {
                        reading.at <= spent && now >= spent.plus_seconds(guard.settle_seconds)
                    });
                    old || spent
                }
            };
            if stale {
                due.push((name.clone(), guard.check.clone()));
            }
        }
    }
    due
}

/// Reads every guard of `due` at `now`.
pub(crate) async fn read_guards(
    inputs: &GuardInputs,
    due: Vec<(String, GuardCheck)>,
    now: Timestamp,
    executor: &BackgroundExecutor,
) -> Fresh {
    let mut fresh = Fresh::default();
    for (pool, check) in due {
        let (value, note) = read_one(inputs, &check, executor).await;
        fresh.readings = std::mem::take(&mut fresh.readings).with(&pool, value, now);
        if let Some(note) = note {
            fresh.notes.insert(pool, note);
        }
    }
    fresh
}

/// Reads the guards of `due` that need no process (memory, disk); a due script guard is left
/// out, so a synchronous caller never waits for one.
pub(crate) fn read_guards_now(
    inputs: &GuardInputs,
    due: Vec<(String, GuardCheck)>,
    now: Timestamp,
) -> Option<GuardReadings> {
    let mut readings = GuardReadings::default();
    for (pool, check) in due {
        let value = match &check {
            GuardCheck::Script(_) => return None,
            _ => quick_value(inputs, &check)?.0,
        };
        readings = readings.with(&pool, value, now);
    }
    Some(readings)
}

fn quick_value(inputs: &GuardInputs, check: &GuardCheck) -> Option<(GuardValue, Option<String>)> {
    match check {
        GuardCheck::FreeMemoryGb(_) => Some(match inputs.readers.free_memory_bytes() {
            Some(bytes) => (GuardValue::FreeMemoryGb(bytes as f64 / GIB), None),
            None => (
                GuardValue::FreeMemoryGb(0.0),
                Some("free memory could not be read".to_string()),
            ),
        }),
        GuardCheck::FreeDiskGb { path, .. } => {
            let path = match path {
                Some(path) => inputs.disk_root.join(path),
                None => inputs.disk_root.clone(),
            };
            Some(match inputs.readers.free_disk_bytes(&path) {
                Some(bytes) => (GuardValue::FreeDiskGb(bytes as f64 / GIB), None),
                None => (
                    GuardValue::FreeDiskGb(0.0),
                    Some(format!("free disk of {} could not be read", path.display())),
                ),
            })
        }
        GuardCheck::Script(_) => None,
    }
}

async fn read_one(
    inputs: &GuardInputs,
    check: &GuardCheck,
    executor: &BackgroundExecutor,
) -> (GuardValue, Option<String>) {
    match check {
        GuardCheck::Script(script) => {
            let run = ScriptRun {
                script: script.clone(),
                dir: inputs.definitions.clone(),
                timeout: inputs.script_timeout,
            };
            match inputs.readers.run_script(run, executor.clone()).await {
                Ok(output) => (
                    GuardValue::ScriptExit(output.exit),
                    (!output.first_line.is_empty()).then_some(output.first_line),
                ),
                Err(reason) => (GuardValue::ScriptExit(-1), Some(reason)),
            }
        }
        _ => quick_value(inputs, check).unwrap_or((GuardValue::ScriptExit(-1), None)),
    }
}
