//! Signed person approvals: the statement a person signs, its line form, the keys that sign it,
//! verification against a set of trusted approvers, and the words every surface shows for an
//! approval's state.
//!
//! This crate is pure: it does no I/O and reads no clock. Callers pass every time in.

mod keys;
mod signer;
mod statement;
mod verify;
mod words;

pub use keys::{
    FixedApprovers, FixedKey, KeyLookup, TrustedApprovers, generate_pkcs8, key_id, public_key,
    sign_with,
};
pub use signer::{ApprovalSigner, SignError};
pub use statement::{ApprovalStatement, Basis, LineError, Method, SignedApproval};
pub use verify::{Expected, RowState, UnverifiedReason, WeakerReason, verify, verify_line};
pub use words::{ApprovalView, Tone, words};

#[cfg(test)]
mod tests;
