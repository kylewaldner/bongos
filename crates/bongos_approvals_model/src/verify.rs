use std::fmt;

use ring::signature::{ED25519, UnparsedPublicKey};

use crate::{Basis, KeyLookup, Method, SignedApproval, TrustedApprovers};

/// What an approval row must be about: its project and change, the author version it approves,
/// and its basis.
///
/// This does not check the statement's `commit`, `amend_patch_id`, `base` or `adopted`; a caller
/// that relies on them (an adopting approval's base and adopted changes, for one) compares them
/// with what it expects itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    pub project_id: String,
    pub change_id: String,
    pub author_patch_id: String,
    pub basis: Basis,
}

/// The verification state of one approval row.
///
/// `approver` is the person named in the signed statement, the one the key is enrolled under.
/// `method` is the signer's own claim of how the key was used, carried in the signed statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    /// Signed with a trusted key of the approver, on this machine's keyring.
    Verified { approver: String, method: Method },
    /// Signed with a trusted key, through a weaker path.
    Weaker {
        approver: String,
        method: Method,
        reason: WeakerReason,
    },
    /// Not shown to be the claimed person's approval.
    Unverified {
        claimed_by: String,
        reason: UnverifiedReason,
    },
}

impl RowState {
    /// The state of a row that carries no signed line.
    pub fn unsigned(claimed_by: impl Into<String>) -> Self {
        RowState::Unverified {
            claimed_by: claimed_by.into(),
            reason: UnverifiedReason::NoSignature,
        }
    }
}

/// Why a verified approval is weaker than one signed on the keyring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WeakerReason {
    Paired,
    TestStore,
}

impl WeakerReason {
    pub fn text(self) -> &'static str {
        match self {
            WeakerReason::Paired => "confirmed by pairing from another machine",
            WeakerReason::TestStore => "test key store (debug build)",
        }
    }
}

impl fmt::Display for WeakerReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

/// Why an approval row did not verify. The texts are fixed words shown to people.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnverifiedReason {
    NoSignature,
    SignatureMismatch,
    UnknownKey,
    NoValidChain,
    KeyRetired,
    OtherChange,
    ApproverMismatch,
}

impl UnverifiedReason {
    pub fn text(self) -> &'static str {
        match self {
            UnverifiedReason::NoSignature => "no signature",
            UnverifiedReason::SignatureMismatch => "signature does not match",
            UnverifiedReason::UnknownKey => "unknown approval key",
            UnverifiedReason::NoValidChain => "approval key has no valid chain",
            UnverifiedReason::KeyRetired => "key retired before the approval",
            UnverifiedReason::OtherChange => "statement is for another change",
            UnverifiedReason::ApproverMismatch => "approver does not match the key",
        }
    }
}

impl fmt::Display for UnverifiedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

/// Verifies a signed approval: the key is trusted (a valid chain, not retired at the statement's
/// time), the signature is the key's over the statement's signing bytes, the statement's approver
/// is the one the key is enrolled under, and the statement is about `expected`.
///
/// The statement's `at` is chosen by the signer, so a retired key can still sign with an earlier
/// `at`; callers must also bound `at` by the row's write time or the change's landing time. The
/// statement's `method` is likewise the signer's claim, not something this check proves.
pub fn verify(
    signed: &SignedApproval,
    trusted: &dyn TrustedApprovers,
    expected: &Expected,
) -> RowState {
    let statement = &signed.statement;
    let unverified = |reason| RowState::Unverified {
        claimed_by: statement.approver.clone(),
        reason,
    };
    let (approver, public_key, retired_at) = match trusted.lookup(&statement.key_id, statement.at) {
        KeyLookup::Trusted {
            approver,
            public_key,
        } => (approver, public_key, None),
        KeyLookup::Retired {
            approver,
            public_key,
            retired_at,
        } => (approver, public_key, Some(retired_at)),
        KeyLookup::NoValidChain => return unverified(UnverifiedReason::NoValidChain),
        KeyLookup::Unknown => return unverified(UnverifiedReason::UnknownKey),
    };
    if crate::key_id(&public_key) != statement.key_id {
        return unverified(UnverifiedReason::UnknownKey);
    }
    if UnparsedPublicKey::new(&ED25519, &public_key)
        .verify(&statement.signing_bytes(), &signed.signature)
        .is_err()
    {
        return unverified(UnverifiedReason::SignatureMismatch);
    }
    if retired_at.is_some_and(|retired_at| retired_at <= statement.at) {
        return unverified(UnverifiedReason::KeyRetired);
    }
    if approver != statement.approver {
        return unverified(UnverifiedReason::ApproverMismatch);
    }
    if statement.project_id != expected.project_id
        || statement.change_id != expected.change_id
        || statement.author_patch_id != expected.author_patch_id
        || statement.basis != expected.basis
    {
        return unverified(UnverifiedReason::OtherChange);
    }
    let method = statement.method.clone();
    let approver = statement.approver.clone();
    match method {
        Method::Keyring => RowState::Verified { approver, method },
        Method::Paired { .. } => RowState::Weaker {
            approver,
            method,
            reason: WeakerReason::Paired,
        },
        Method::TestStore => RowState::Weaker {
            approver,
            method,
            reason: WeakerReason::TestStore,
        },
    }
}

/// Verifies a row's stored line. A row with no line has "no signature"; a line that cannot be
/// read is "signature does not match"; a line signed by someone other than the row's
/// `claimed_by` is "approver does not match the key". The caveats on [`verify`] apply.
pub fn verify_line(
    line: Option<&str>,
    claimed_by: &str,
    trusted: &dyn TrustedApprovers,
    expected: &Expected,
) -> RowState {
    let Some(line) = line else {
        return RowState::unsigned(claimed_by);
    };
    match SignedApproval::from_line(line) {
        Ok(signed) if signed.statement.approver != claimed_by => RowState::Unverified {
            claimed_by: claimed_by.to_string(),
            reason: UnverifiedReason::ApproverMismatch,
        },
        Ok(signed) => verify(&signed, trusted, expected),
        Err(_) => RowState::Unverified {
            claimed_by: claimed_by.to_string(),
            reason: UnverifiedReason::SignatureMismatch,
        },
    }
}
