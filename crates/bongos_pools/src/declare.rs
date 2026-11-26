//! A declared pool as a store keeps it, and how the declarations of several projects share one
//! machine pool.
//!
//! A pool's effective values, last wins: the user setting (only `settle_seconds`, when the
//! declaration omits it), the declaration, the project profile (only `size`, passed by the
//! caller). A pool declared again keeps its holds, waiters and guard reading; only its kind, size
//! and guard check change.

use std::time::Duration;

use bongos_pools_model::{GuardCheck, PoolDecl, PoolKind, PoolScope, PoolState};
use serde::{Deserialize, Serialize};

use crate::PoolsSettings;

/// The user settings a declaration falls back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclDefaults {
    /// `pools.guard_settle_seconds`.
    pub settle_seconds: u32,
}

impl DeclDefaults {
    pub fn from_settings(settings: &PoolsSettings) -> DeclDefaults {
        DeclDefaults {
            settle_seconds: seconds(settings.guard_settle),
        }
    }
}

fn seconds(duration: Duration) -> u32 {
    u32::try_from(duration.as_secs()).unwrap_or(u32::MAX)
}

/// The effective values of one declared pool.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolConfig {
    pub name: String,
    pub kind: PoolKind,
    /// The slots of a `slots` pool; 1 for a mutex; unused by `rw` and `guard`.
    pub size: u32,
    pub guard: Option<GuardCheck>,
    pub settle_seconds: u32,
}

impl PoolConfig {
    /// The effective values of `decl`: `profile_size` (the project profile's size for it) over
    /// the declared size (1 when none), the declared `settle_seconds` over the setting's.
    pub fn from_decl(
        decl: &PoolDecl,
        profile_size: Option<u32>,
        defaults: DeclDefaults,
    ) -> PoolConfig {
        let size = match decl.kind {
            PoolKind::Mutex => 1,
            _ => profile_size.or(decl.size).unwrap_or(1),
        };
        PoolConfig {
            name: decl.name.clone(),
            kind: decl.kind,
            size,
            guard: decl.guard.as_ref().map(|guard| guard.check.clone()),
            settle_seconds: decl.settle_seconds.unwrap_or(defaults.settle_seconds),
        }
    }

    /// The configs of the declarations of `scope`, each sized by `profile_size`.
    pub fn of_scope(
        decls: &[PoolDecl],
        scope: PoolScope,
        profile_size: impl Fn(&str) -> Option<u32>,
        defaults: DeclDefaults,
    ) -> Vec<PoolConfig> {
        decls
            .iter()
            .filter(|decl| decl.scope == scope)
            .map(|decl| PoolConfig::from_decl(decl, profile_size(&decl.name), defaults))
            .collect()
    }

    /// A new, empty pool with these values.
    pub fn state(&self) -> PoolState {
        let state = PoolState::new(self.kind, self.size);
        match &self.guard {
            Some(check) => state.with_guard(check.clone(), self.settle_seconds),
            None => state,
        }
    }

    /// Gives `state` these values, keeping its holds, its waiters and its guard's last reading
    /// and spent time while it keeps a guard. A lowered size lets the holds finish and grants
    /// nothing until they fall below it.
    pub fn apply_to(&self, state: &mut PoolState) {
        let fresh = self.state();
        state.kind = fresh.kind;
        state.size = fresh.size;
        state.guard = match (state.guard.take(), fresh.guard) {
            (Some(mut old), Some(new)) => {
                if old.check != new.check {
                    // A reading of another check says nothing about this one.
                    old.reading = None;
                }
                old.check = new.check;
                old.settle_seconds = new.settle_seconds;
                Some(old)
            }
            (_, new) => new,
        };
    }
}

/// A project as the machine store names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

/// One project's declaration of a machine pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub project: ProjectRef,
    pub kind: PoolKind,
    pub size: u32,
}

/// What a declaration changed beside the pools it registered.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeclareOutcome {
    /// Pools this project declares with another size than the one that stands.
    pub warnings: Vec<SizeWarning>,
    /// Pools this project declares with another kind: not registered; its requests are refused.
    pub refused: Vec<KindClash>,
}

/// "machine pool gpu: this project declares 2, project Invoicing set 1".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeWarning {
    pub pool: String,
    pub declared: u32,
    pub stands: u32,
    /// The project whose size stands; `None` in the project store.
    pub set_by: Option<ProjectRef>,
}

impl std::fmt::Display for SizeWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.set_by {
            Some(set_by) => write!(
                f,
                "machine pool {}: this project declares {}, project {} set {}",
                self.pool, self.declared, set_by.name, self.stands
            ),
            None => write!(
                f,
                "pool {}: declared {}, {} stands",
                self.pool, self.declared, self.stands
            ),
        }
    }
}

/// A pool declared with another kind than the one that stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindClash {
    pub pool: String,
    pub declared: PoolKind,
    pub stands: PoolKind,
    /// The project that set the kind; `None` in the project store, where a pool changes kind
    /// only while nobody holds or waits for it.
    pub set_by: Option<ProjectRef>,
}

impl std::fmt::Display for KindClash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.set_by {
            Some(set_by) => write!(
                f,
                "machine pool {} is a {} pool, set by project {}; this project declares it {}",
                self.pool,
                self.stands.word(),
                set_by.name,
                self.declared.word()
            ),
            None => write!(
                f,
                "pool {} is in use as a {} pool; it becomes {} once nobody holds or waits for it",
                self.pool,
                self.stands.word(),
                self.declared.word()
            ),
        }
    }
}
