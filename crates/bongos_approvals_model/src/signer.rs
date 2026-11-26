use futures::future::BoxFuture;

use crate::{ApprovalStatement, SignedApproval};

/// Signs approval statements with a person's key. Only a window registers one; a headless host has
/// none, so the engine never signs.
pub trait ApprovalSigner: Send + Sync {
    fn sign(
        &self,
        statement: ApprovalStatement,
    ) -> BoxFuture<'static, Result<SignedApproval, SignError>>;
}

/// Why a statement could not be signed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    /// This machine has no usable approval key store.
    #[error("no approval key store: {0}")]
    NoKeyStore(String),
    /// The key store was there but signing failed.
    #[error("signing the approval failed: {0}")]
    Failed(String),
}
