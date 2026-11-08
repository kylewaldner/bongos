//! A stable digest of the artifact list a chat was given, so the record can tell later that the
//! list changed without keeping any path. It is written to disk, so it must not depend on the
//! process: `std`'s hashers are seeded per process, which is why this is FNV-1a 64 written out.

use bongos_work_model::SharedArtifact;

pub const DIGEST_PREFIX: &str = "fnv1a64:";

const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// `fnv1a64:<16 hex digits>` over the artifacts sorted by path, then description. Each path and
/// description is hashed with its length in front, so no two lists share a byte stream. The order
/// of `artifacts` does not matter.
pub fn context_digest(artifacts: &[SharedArtifact]) -> String {
    let mut pairs: Vec<(String, &str)> = artifacts
        .iter()
        .map(|artifact| {
            (
                artifact.path.to_string_lossy().into_owned(),
                artifact.description.as_str(),
            )
        })
        .collect();
    pairs.sort();

    let mut bytes = Vec::new();
    for (path, description) in &pairs {
        for part in [path.as_str(), description] {
            bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
    }
    format!("{DIGEST_PREFIX}{:016x}", fnv1a64(&bytes))
}
