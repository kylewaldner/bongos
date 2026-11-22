//! Desktop notifications. [`desktop`] sends one through the process's sink: `notify-send` on
//! Linux and `osascript` on macOS when found on `PATH`, otherwise a log line. A gate notifies
//! exactly once: [`GateNotifier`] records `notified/<run id>/<node key>` in the lanes namespace
//! with the first notification, so repeated events and a restarted host stay quiet.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bongos_project_repo::StateTx;
use smol::process::Command;

use crate::store::LANES_NAMESPACE;

/// The key prefix of the gate notification records, followed by `<run id>/<node key>`.
pub const NOTIFIED_PREFIX: &str = "notified/";

/// Where a notification goes.
pub trait DesktopSink: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// The system sink: the platform's notifier when found on `PATH`, else a log line.
pub struct SystemSink;

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

impl DesktopSink for SystemSink {
    fn notify(&self, title: &str, body: &str) {
        let command = if cfg!(target_os = "macos") {
            on_path("osascript").map(|program| {
                let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
                let mut command = Command::new(program);
                command.arg("-e").arg(format!(
                    "display notification \"{}\" with title \"{}\"",
                    escape(body),
                    escape(title)
                ));
                command
            })
        } else {
            on_path("notify-send").map(|program| {
                let mut command = Command::new(program);
                command.arg(title).arg(body);
                command
            })
        };
        match command {
            Some(mut command) => {
                if let Err(error) = command.spawn() {
                    log::warn!("lanes: the desktop notification failed: {error}: {title}: {body}");
                }
            }
            None => log::info!("lanes: {title}: {body}"),
        }
    }
}

/// Sends a desktop notification through the system sink.
pub fn desktop(title: &str, body: &str) {
    SystemSink.notify(title, body);
}

/// Notifies once per gate.
#[derive(Clone)]
pub struct GateNotifier {
    sink: Arc<dyn DesktopSink>,
}

impl GateNotifier {
    pub fn new(sink: Arc<dyn DesktopSink>) -> Self {
        Self { sink }
    }

    /// A notifier over the system sink.
    pub fn system() -> Self {
        Self::new(Arc::new(SystemSink))
    }

    /// Records that the gate `node_key` of run `run_id` waits on a person, unless it did before.
    /// The record (the time `now`) commits with the transaction. Returns the notification to
    /// [`send`](Self::send) once the transaction has committed, or `None` when the gate was
    /// notified before. Nothing is sent from inside the transaction, so a rolled-back record
    /// never leaves a notification behind.
    pub fn notify_gate(
        &self,
        tx: &mut StateTx,
        run_id: &str,
        node_key: &str,
        title: &str,
        body: &str,
        now: &str,
    ) -> Result<Option<PendingNotification>> {
        let key = format!("{NOTIFIED_PREFIX}{run_id}/{node_key}");
        let mut kv = tx.kv(LANES_NAMESPACE);
        if kv.get(&key)?.is_some() {
            return Ok(None);
        }
        kv.put(&key, toml::Value::String(now.to_string()))?;
        Ok(Some(PendingNotification {
            title: title.to_string(),
            body: body.to_string(),
        }))
    }

    /// Sends a notification that [`notify_gate`](Self::notify_gate) returned.
    pub fn send(&self, pending: &PendingNotification) {
        self.sink.notify(&pending.title, &pending.body);
    }
}

/// A gate notification recorded in a transaction and not sent yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingNotification {
    pub title: String,
    pub body: String,
}
