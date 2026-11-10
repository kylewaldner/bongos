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

use bongos_linear_model::{ApiError, IssueSummary, LinearApi};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Entity, Global};
use settings::Settings as _;

use crate::LinearSettings;

/// What a request is for. Only these may pass the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// A read or write about a task that is linked to a ticket, by the task's id.
    LinkedTask(&'a str),
    /// One read of several linked tasks' tickets, by the tasks' ids: a refresh batch.
    LinkedTasks(&'a [String]),
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
    /// Linear rejected the token earlier in this session; nothing is sent until a token is set.
    Rejected,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::Disabled => "Linear sync is off (linear.enabled)",
            Refusal::NoToken => "No Linear token: run linear: set token, or set LINEAR_API_KEY",
            Refusal::NotLinked => "This task is not linked to a Linear ticket",
            Refusal::NoClient => "The Linear client is not available",
            Refusal::Rejected => "Linear rejected the token; run linear: set token",
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

/// Why automatic reads are paused after a rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Limit {
    /// Unix time in milliseconds when Linear lets requests through again, when it said.
    reset_at: Option<u64>,
}

#[derive(Default)]
pub struct LinearSync {
    client: Option<Arc<dyn LinearApi>>,
    token_present: bool,
    /// Linear answered `Unauthorized`: nothing more is sent until a token is set.
    rejected: bool,
    limit: Option<Limit>,
    linked: BTreeSet<String>,
    reads: BTreeMap<String, ReadState>,
    /// When Linear last answered for the task's ticket (found or not), Unix milliseconds. A failed
    /// read leaves it alone.
    answered: BTreeMap<String, u64>,
    /// Per linked group, the tickets under its ticket that no task links (D12).
    suggestions: BTreeMap<String, Vec<IssueSummary>>,
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
        if !present {
            self.rejected = false;
        }
    }

    /// A token Linear accepted was just stored: requests may go out again.
    pub fn accept_token(&mut self, client: Arc<dyn LinearApi>) {
        self.set_token(true, Some(client));
        self.rejected = false;
        self.limit = None;
    }

    /// Notes that Linear rejected the token. Returns true the first time, for the one toast.
    pub fn mark_rejected(&mut self) -> bool {
        !std::mem::replace(&mut self.rejected, true)
    }

    /// Notes a rate limit, with the reset time (Unix milliseconds) when Linear sent one.
    pub fn mark_limited(&mut self, reset_at: Option<u64>) {
        self.limit = Some(Limit { reset_at });
    }

    /// A read went through: the limit, if any, is over.
    pub fn clear_limit(&mut self) {
        self.limit = None;
    }

    /// When the current rate limit ends, if one is in force and Linear said when.
    pub fn limit_reset(&self) -> Option<u64> {
        self.limit.and_then(|limit| limit.reset_at)
    }

    /// Whether an automatic refresh may run now: not after a rejected token, and not while a
    /// rate limit is in force (only an explicit command tries then).
    pub fn auto_allowed(&self, now_ms: u64) -> bool {
        if self.rejected {
            return false;
        }
        match self.limit {
            None => true,
            Some(Limit { reset_at }) => reset_at.is_some_and(|reset| now_ms >= reset),
        }
    }

    pub fn token_present(&self) -> bool {
        self.token_present
    }

    /// Replaces the ids of the tasks that are linked to a ticket.
    pub fn set_linked(&mut self, ids: impl IntoIterator<Item = String>) {
        self.linked = ids.into_iter().collect();
        self.reads.retain(|id, _| self.linked.contains(id));
        self.answered.retain(|id, _| self.linked.contains(id));
        self.suggestions.retain(|id, _| self.linked.contains(id));
    }

    /// Whether `task_id` is a linked task.
    pub fn is_linked(&self, task_id: &str) -> bool {
        self.linked.contains(task_id)
    }

    /// Sets how the read of `task_id`'s ticket went at `now_ms`. Ignored for a task that is not
    /// linked.
    pub fn set_read(&mut self, task_id: &str, state: ReadState, now_ms: u64) {
        if !self.linked.contains(task_id) {
            return;
        }
        self.reads.insert(task_id.to_string(), state);
        if matches!(state, ReadState::Fresh { .. } | ReadState::NotFound) {
            self.answered.insert(task_id.to_string(), now_ms);
        }
    }

    /// When Linear last answered for `task_id`'s ticket, found or not.
    pub fn answered_at(&self, task_id: &str) -> Option<u64> {
        self.answered.get(task_id).copied()
    }

    /// The suggestions of the linked group `group_id`: tickets under its ticket that no task links.
    pub fn suggestions(&self, group_id: &str) -> &[IssueSummary] {
        self.suggestions.get(group_id).map_or(&[], Vec::as_slice)
    }

    /// Replaces the suggestions of the groups in `scope` with `found` (a group in `scope` that
    /// is not in `found` has none).
    pub fn replace_suggestions(
        &mut self,
        scope: &[String],
        mut found: BTreeMap<String, Vec<IssueSummary>>,
    ) {
        for id in scope {
            match found.remove(id).filter(|_| self.linked.contains(id)) {
                Some(list) => {
                    self.suggestions.insert(id.clone(), list);
                }
                None => {
                    self.suggestions.remove(id);
                }
            }
        }
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
        if matches!(state, ReadState::Fresh { .. } | ReadState::NotFound) {
            self.answered.insert(task_id.to_string(), now_ms);
        }
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
        } else if self.rejected && !candidate {
            Err(Refusal::Rejected)
        } else if !candidate && self.client.is_none() {
            Err(Refusal::NoClient)
        } else if match scope {
            Scope::LinkedTask(id) => !self.linked.contains(id),
            Scope::LinkedTasks(ids) => {
                ids.is_empty() || ids.iter().any(|id| !self.linked.contains(id))
            }
            _ => false,
        } {
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
