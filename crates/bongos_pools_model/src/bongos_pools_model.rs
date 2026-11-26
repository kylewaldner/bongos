//! Declaration model for pools, queues and triggers: the three declaration files parsed with line
//! numbers (an unknown key is an error), the grammar of a `needs` entry, schedule parsing, and the
//! per-folder validation that names file and line; the grant rule over a table of pools, and the
//! holders of those pools with their liveness.
//!
//! This crate is plain Rust: no gpui, no store, no file access. A missing file is the empty text.

pub mod decl;
pub mod grant;
pub mod holder;
pub mod needs;
pub mod schedule;
pub mod source;
pub mod validate;

pub use decl::{
    Action, AllDone, ExhaustedAction, FieldType, Guard, GuardCheck, ItemField, LaneFields, On,
    Parsed, PoolDecl, PoolKind, PoolScope, QueueDecl, RunResult, TriggerDecl, lane_fields,
    parse_pools, parse_queues, parse_triggers,
};
pub use grant::{
    Blocked, Grant, GrantedPool, GuardReading, GuardReadings, GuardState, GuardValue, Hold,
    PoolState, PoolTable, PoolWait, QueueError, Request, RequestId, Timestamp, WaitStatus, Waiter,
    admit_now, grant, try_now, wait_status,
};
pub use holder::{
    Holder, HolderLabel, HolderPath, HostRef, Level, OwnerKey, Probe, ProcRef, Pruned, alive,
    prune, waiter_alive,
};
pub use needs::{Need, NeedMode, parse_need};
pub use schedule::{
    Schedule, TimeOfDay, Weekday, parse_duration, parse_every, parse_time, parse_timer,
    parse_weekday,
};
pub use source::{Diagnostic, Located, ParseError, SCHEMA_VERSION};
pub use validate::{Problem, validate_pools, validate_queues, validate_triggers};

#[cfg(test)]
mod decl_tests;
#[cfg(test)]
mod grant_prop_tests;
#[cfg(test)]
mod grant_tests;
#[cfg(test)]
mod holder_tests;
#[cfg(test)]
mod needs_tests;
#[cfg(test)]
mod schedule_tests;
#[cfg(test)]
mod validate_tests;
