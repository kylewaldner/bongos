//! The seam every state write of a project goes through, so nothing assumes this process is the
//! project's writer.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use bongos_project_repo_model::Author;
use gpui::{BackgroundExecutor, Task};
use serde::{Deserialize, Serialize};

use crate::landing::LandRequest;
use crate::state::StateWriter;
use crate::state_ops::StateOp;

/// A repository call that writes state. Each variant arrives with the step that adds its
/// operation. A command carries no person's attestation (it cannot be serialized), only a
/// [`PersonClaim`], which the host trusts only from a peer it verified (`HostBoundary`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RepoCommand {
    /// `project: land` of a change.
    Land {
        change_id: String,
        request: LandRequest,
        person: Option<PersonClaim>,
    },
    /// `project: approve` of a change.
    Approve {
        change_id: String,
        person: PersonClaim,
    },
}

/// A window's claim that a person at its keyboard asked for a command, by the person's name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonClaim {
    pub name: String,
}

/// The answer to a [`RepoCommand`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RepoReply {
    Done,
}

/// Where one open project's writes go: the writer in this process, another process's writer, or
/// nowhere.
pub trait StateSink: Send + Sync {
    /// Applies `ops` in one transaction. `origin` only labels history.
    fn apply(&self, ops: Vec<StateOp>, origin: Author) -> Task<Result<()>>;

    fn repo(&self, command: RepoCommand) -> Task<Result<RepoReply>>;
}

/// The sink of the process that holds the writer.
pub struct LocalSink {
    writer: Arc<StateWriter>,
    executor: BackgroundExecutor,
}

impl LocalSink {
    pub fn new(writer: Arc<StateWriter>, executor: BackgroundExecutor) -> Self {
        Self { writer, executor }
    }
}

impl StateSink for LocalSink {
    fn apply(&self, ops: Vec<StateOp>, origin: Author) -> Task<Result<()>> {
        let writer = self.writer.clone();
        self.executor.spawn(async move {
            let notes = writer.transact_with(&origin, &ops, |tx| tx.apply_ops(&ops))?;
            for note in notes {
                log::info!("{note}");
            }
            Ok(())
        })
    }

    fn repo(&self, command: RepoCommand) -> Task<Result<RepoReply>> {
        // A window of this process calls the repository itself, with the person's marker; a
        // command, which carries only a claim, is for the host boundary of another process.
        Task::ready(Err(anyhow!(
            "{command:?} carries no attestation here; call the repository directly"
        )))
    }
}

/// The sink of a process that found the project's writer held by another process, until writes
/// can be sent there: it refuses every write.
pub struct RefusingSink {
    pid: Option<u32>,
}

impl RefusingSink {
    /// `pid` is the process that holds the writer, `None` when it could not be read.
    pub fn new(pid: impl Into<Option<u32>>) -> Self {
        Self { pid: pid.into() }
    }

    fn refusal(&self) -> anyhow::Error {
        let pid = match self.pid {
            Some(pid) => format!("pid {pid}"),
            None => "pid unknown".to_string(),
        };
        anyhow!("this project is open in another bongos process ({pid}); work there or close it")
    }
}

impl StateSink for RefusingSink {
    fn apply(&self, _ops: Vec<StateOp>, _origin: Author) -> Task<Result<()>> {
        Task::ready(Err(self.refusal()))
    }

    fn repo(&self, _command: RepoCommand) -> Task<Result<RepoReply>> {
        Task::ready(Err(self.refusal()))
    }
}
