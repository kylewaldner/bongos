//! A person's consent to run one run's programs without the OS sandbox. The grant lives in the
//! writer's memory only ([`RunConsentStore`]): nothing on disk grants, so a reload, a crash or a
//! handover starts with an empty store and the next unsandboxed start asks again. The run record
//! keeps the question (`ConsentAsked`) and every answer (`ConsentAnswered`), never the grant.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use bongos_workflow_model::RunState;
use bongos_workflow_model::unsandboxed::{self, AllowedBy};
use chrono::{DateTime, SecondsFormat, Utc};

/// A person's yes to one run going without the OS sandbox: when (RFC 3339) and who.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub at: String,
    pub by: String,
}

impl Grant {
    /// The grant as the permit's reason for an unsandboxed start.
    pub fn allowed_by(&self) -> AllowedBy {
        AllowedBy::RunConsent {
            at: self.at.clone(),
            by: self.by.clone(),
        }
    }
}

/// What a grant is for: going without the OS sandbox, or a rule of a policy that asks a person
/// once per run, by the rule's id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsentScope {
    Sandbox,
    Rule(String),
}

/// Where the grants of the live runs are kept, per run and scope. The runtime forgets every
/// scope of a run when the run ends or is dropped.
pub trait RunConsentStore: Send + Sync {
    fn granted(&self, run: &str, scope: &ConsentScope) -> Option<Grant>;
    fn grant(&self, run: &str, scope: ConsentScope, grant: Grant);
    /// Forgets every scope of `run`.
    fn forget(&self, run: &str);
}

/// The default store: a map in memory, gone with the process.
#[derive(Default)]
pub struct InMemoryRunConsents(Mutex<BTreeMap<(String, ConsentScope), Grant>>);

impl RunConsentStore for InMemoryRunConsents {
    fn granted(&self, run: &str, scope: &ConsentScope) -> Option<Grant> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(run.to_string(), scope.clone()))
            .cloned()
    }

    fn grant(&self, run: &str, scope: ConsentScope, grant: Grant) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((run.to_string(), scope), grant);
    }

    fn forget(&self, run: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(granted_run, _), _| granted_run != run);
    }
}

static INSTALLED: RwLock<Option<Arc<dyn RunConsentStore>>> = RwLock::new(None);

/// The store a new runtime takes in place of its own [`InMemoryRunConsents`]. Nothing in this
/// crate installs one: it is the seam for a later store that also answers a policy's per-run
/// rules, and a test sets a runtime's store with `Runtime::set_consents` instead, since this one
/// is process-wide. A runtime made before the call keeps the store it has.
pub struct RunConsents;

impl RunConsents {
    pub fn set(store: Option<Arc<dyn RunConsentStore>>) {
        *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = store;
    }

    /// The installed store, else a new empty one.
    pub(crate) fn store() -> Arc<dyn RunConsentStore> {
        INSTALLED
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryRunConsents::default()))
    }
}

/// The wall clock an answer's time is read from; tests set a fixed one.
pub type WallClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The system's wall clock.
pub fn system_clock() -> WallClock {
    Arc::new(Utc::now)
}

/// `time` as the RFC 3339 text an answer records, in UTC with second precision.
pub fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The probe's reason in a `SandboxRequired` failure: `decide`'s ask message wraps it as
/// "no OS sandbox is available (<reason>)"; any other text is taken whole.
pub fn probe_reason(detail: &str) -> &str {
    detail
        .strip_prefix("no OS sandbox is available (")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(detail)
}

/// The question a person sees for the run's pending ask, naming its first held node; `None`
/// when nothing is pending.
pub fn question(state: &RunState) -> Option<String> {
    let pending = state.consent_pending.as_ref()?;
    let node = pending.keys.first().map_or("", |key| key.node.as_str());
    Some(unsandboxed::ask_text(node, &pending.reason))
}

#[cfg(test)]
#[path = "consent_tests.rs"]
pub(crate) mod tests;
