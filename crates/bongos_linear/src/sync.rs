//! [`LinearSync`]: the one global entity that owns the Linear client and decides whether a
//! request may leave the machine.
//!
//! The promise of this step: no request goes out unless `linear.enabled` is true, a token exists
//! and the request is for a linked task or is an explicit Linear command ([`Scope`]). The client
//! is private to the entity. The only ways to use it are [`LinearSync::call`] and
//! [`LinearSync::verify_token`], and both pass through the one gate. A refused request is a typed
//! [`Refusal`] and never reaches the client.
//!
//! It also keeps, per linked task, how the last read of its ticket went ([`ReadState`]), and the
//! counts of requests allowed and refused, for the state dump. It holds no token: the client
//! holds that, and the entity only knows whether one exists.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use bongos_linear_model::{ApiError, LinearApi};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Entity, Global};
use settings::Settings as _;

use crate::LinearSettings;

/// What a request is for. Only these may pass the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// A read or write about a task that is linked to a ticket, by the task's id.
    LinkedTask(&'a str),
    /// `task: new from linear`.
    NewFromLinear,
    /// `task: link to linear`.
    LinkToLinear,
    /// `linear: set token`: the one request that checks a token before it is stored.
    SetToken,
}

/// Why the gate refused a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `linear.enabled` is false.
    Disabled,
    /// No token is stored and `LINEAR_API_KEY` is unset.
    NoToken,
    /// The task is not linked to a ticket.
    NotLinked,
    /// A token exists but no client was built for it.
    NoClient,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::Disabled => "Linear sync is off (linear.enabled)",
            Refusal::NoToken => "No Linear token: run linear: set token, or set LINEAR_API_KEY",
            Refusal::NotLinked => "This task is not linked to a Linear ticket",
            Refusal::NoClient => "The Linear client is not available",
        })
    }
}

/// How the last read of a task's ticket went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadState {
    /// Read at this Unix time in milliseconds.
    Fresh {
        at: u64,
    },
    Unreachable,
    RateLimited,
    NotFound,
}

impl ReadState {
    /// The state a failed read leaves.
    pub fn from_error(error: &ApiError) -> Self {
        match error {
            ApiError::RateLimited { .. } => Self::RateLimited,
            ApiError::NotFound(_) => Self::NotFound,
            ApiError::Unauthorized
            | ApiError::Network(_)
            | ApiError::Server(_)
            | ApiError::Duplicate(_) => Self::Unreachable,
        }
    }
}

impl fmt::Display for ReadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fresh { at } => write!(f, "fresh at {at}"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::RateLimited => f.write_str("rate limited"),
            Self::NotFound => f.write_str("not found"),
        }
    }
}

/// What the dump shows. Holds no token, not even its length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub enabled: bool,
    pub token_present: bool,
    pub requests_allowed: u64,
    pub requests_refused: u64,
    pub linked: Vec<String>,
    pub reads: Vec<(String, ReadState)>,
}

#[derive(Default)]
pub struct LinearSync {
    client: Option<Arc<dyn LinearApi>>,
    token_present: bool,
    linked: BTreeSet<String>,
    reads: BTreeMap<String, ReadState>,
    allowed: u64,
    refused: u64,
}

struct GlobalSync(Entity<LinearSync>);

impl Global for GlobalSync {}

/// Creates the global entity. Call once, after the settings are registered.
pub fn install(cx: &mut App) {
    let entity = cx.new(|_| LinearSync::default());
    cx.set_global(GlobalSync(entity));
}

/// The global entity, when [`install`] ran.
pub fn global(cx: &App) -> Option<Entity<LinearSync>> {
    cx.try_global::<GlobalSync>().map(|global| global.0.clone())
}

impl LinearSync {
    /// Records what is known about the token: `None` for no token, else the client built with it.
    /// `present` without a client is a token the factory could not use.
    pub fn set_token(&mut self, present: bool, client: Option<Arc<dyn LinearApi>>) {
        self.token_present = present;
        self.client = if present { client } else { None };
    }

    pub fn token_present(&self) -> bool {
        self.token_present
    }

    /// Replaces the ids of the tasks that are linked to a ticket.
    pub fn set_linked(&mut self, ids: impl IntoIterator<Item = String>) {
        self.linked = ids.into_iter().collect();
        self.reads.retain(|id, _| self.linked.contains(id));
    }

    pub fn read_state(&self, task_id: &str) -> Option<ReadState> {
        self.reads.get(task_id).copied()
    }

    /// Notes how the read of `task_id`'s ticket went.
    pub fn record_read<T>(&mut self, task_id: &str, result: &Result<T, ApiError>, now_ms: u64) {
        let state = match result {
            Ok(_) => ReadState::Fresh { at: now_ms },
            Err(error) => ReadState::from_error(error),
        };
        self.reads.insert(task_id.to_string(), state);
    }

    pub fn snapshot(&self, cx: &App) -> Snapshot {
        Snapshot {
            enabled: LinearSettings::get_global(cx).enabled,
            token_present: self.token_present,
            requests_allowed: self.allowed,
            requests_refused: self.refused,
            linked: self.linked.iter().cloned().collect(),
            reads: self.reads.iter().map(|(id, s)| (id.clone(), *s)).collect(),
        }
    }

    /// The gate. `candidate` is true for a token that is being checked and not yet stored.
    fn gate(&mut self, scope: Scope, candidate: bool, cx: &App) -> Result<(), Refusal> {
        let verdict = if !LinearSettings::get_global(cx).enabled {
            Err(Refusal::Disabled)
        } else if !self.token_present && !candidate {
            Err(Refusal::NoToken)
        } else if !candidate && self.client.is_none() {
            Err(Refusal::NoClient)
        } else if matches!(scope, Scope::LinkedTask(id) if !self.linked.contains(id)) {
            Err(Refusal::NotLinked)
        } else {
            Ok(())
        };
        match verdict {
            Ok(()) => self.allowed += 1,
            Err(_) => self.refused += 1,
        }
        verdict
    }

    /// Starts a request through the gate: `request` gets the client and returns the future of the
    /// call. A refusal returns before the client is touched.
    pub fn call<T>(
        &mut self,
        scope: Scope,
        cx: &App,
        request: impl FnOnce(&dyn LinearApi) -> BoxFuture<'static, T>,
    ) -> Result<BoxFuture<'static, T>, Refusal> {
        self.gate(scope, false, cx)?;
        let client = self.client.clone().ok_or(Refusal::NoClient)?;
        Ok(request(&*client))
    }

    /// Checks a token that is not stored yet: the `viewer()` request of `candidate`, a client
    /// built with it, under [`Scope::SetToken`].
    pub fn verify_token(
        &mut self,
        candidate: &dyn LinearApi,
        cx: &App,
    ) -> Result<BoxFuture<'static, Result<String, ApiError>>, Refusal> {
        self.gate(Scope::SetToken, true, cx)?;
        Ok(candidate.viewer())
    }
}
