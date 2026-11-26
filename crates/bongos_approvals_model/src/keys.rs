use chrono::{DateTime, Utc};
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};

use crate::{ApprovalStatement, SignError, SignedApproval};

/// The id of an approval key: the first 16 hex digits of the SHA-256 of its public key.
pub fn key_id(public_key: &[u8]) -> String {
    hex(&digest(&SHA256, public_key).as_ref()[..8])
}

/// A new Ed25519 key pair as PKCS#8 bytes, the form the key stores keep.
pub fn generate_pkcs8() -> Result<Vec<u8>, SignError> {
    Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map(|document| document.as_ref().to_vec())
        .map_err(|_| SignError::Failed("could not generate an approval key".into()))
}

/// The public key of a PKCS#8 Ed25519 key pair.
pub fn public_key(pkcs8: &[u8]) -> Result<Vec<u8>, SignError> {
    Ok(key_pair(pkcs8)?.public_key().as_ref().to_vec())
}

/// Signs `statement` with a PKCS#8 Ed25519 key pair. An empty `key_id` is filled in from the key;
/// a `key_id` that names another key is refused.
pub fn sign_with(
    pkcs8: &[u8],
    mut statement: ApprovalStatement,
) -> Result<SignedApproval, SignError> {
    let pair = key_pair(pkcs8)?;
    let id = key_id(pair.public_key().as_ref());
    if statement.key_id.is_empty() {
        statement.key_id = id;
    } else if statement.key_id != id {
        return Err(SignError::Failed(format!(
            "the statement names the key {} but the signing key is {id}",
            statement.key_id
        )));
    }
    let signature = pair.sign(&statement.signing_bytes()).as_ref().to_vec();
    Ok(SignedApproval {
        statement,
        signature,
    })
}

fn key_pair(pkcs8: &[u8]) -> Result<Ed25519KeyPair, SignError> {
    Ed25519KeyPair::from_pkcs8(pkcs8)
        .map_err(|_| SignError::Failed("the approval key is not a valid Ed25519 key".into()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What the trusted approvers know about a key id at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyLookup {
    /// Enrolled under `approver` with a valid chain to the root, and not retired.
    Trusted {
        approver: String,
        public_key: Vec<u8>,
    },
    /// Enrolled with a valid chain, and retired at `retired_at`.
    Retired {
        approver: String,
        public_key: Vec<u8>,
        retired_at: DateTime<Utc>,
    },
    /// Listed, but with no valid chain of signatures to the root.
    NoValidChain,
    /// Not listed.
    Unknown,
}

/// The keys verification trusts, and who each belongs to.
pub trait TrustedApprovers: Send + Sync {
    /// What is known about `key_id` for an approval made at `at`.
    fn lookup(&self, key_id: &str, at: DateTime<Utc>) -> KeyLookup;
    /// A digest of every entry, recorded with each landing.
    fn fingerprint(&self) -> String;
    /// The key id of the chain's root, when there is one.
    fn root_key_id(&self) -> Option<String>;
}

/// One key of [`FixedApprovers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixedKey {
    pub approver: String,
    pub public_key: Vec<u8>,
    pub retired_at: Option<DateTime<Utc>>,
    /// Whether the key has a valid chain to the root.
    pub chained: bool,
}

impl FixedKey {
    /// A trusted, chained, unretired key.
    pub fn trusted(approver: impl Into<String>, public_key: Vec<u8>) -> Self {
        FixedKey {
            approver: approver.into(),
            public_key,
            retired_at: None,
            chained: true,
        }
    }
}

/// Trusted approvers from a fixed list, for tests and for callers that already hold the list.
/// The first key is the root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixedApprovers {
    pub keys: Vec<FixedKey>,
}

impl FixedApprovers {
    pub fn new(keys: Vec<FixedKey>) -> Self {
        FixedApprovers { keys }
    }
}

impl TrustedApprovers for FixedApprovers {
    fn lookup(&self, id: &str, _at: DateTime<Utc>) -> KeyLookup {
        let Some(key) = self.keys.iter().find(|key| key_id(&key.public_key) == id) else {
            return KeyLookup::Unknown;
        };
        if !key.chained {
            return KeyLookup::NoValidChain;
        }
        match key.retired_at {
            Some(retired_at) => KeyLookup::Retired {
                approver: key.approver.clone(),
                public_key: key.public_key.clone(),
                retired_at,
            },
            None => KeyLookup::Trusted {
                approver: key.approver.clone(),
                public_key: key.public_key.clone(),
            },
        }
    }

    fn fingerprint(&self) -> String {
        let mut text = String::new();
        for key in &self.keys {
            let retired = key.retired_at.map(|at| at.to_rfc3339()).unwrap_or_default();
            text.push_str(&format!(
                "{} {} {} {retired} {}\n",
                key.approver.escape_default(),
                key_id(&key.public_key),
                hex(&key.public_key),
                key.chained
            ));
        }
        hex(digest(&SHA256, text.as_bytes()).as_ref())
    }

    fn root_key_id(&self) -> Option<String> {
        self.keys.first().map(|key| key_id(&key.public_key))
    }
}
