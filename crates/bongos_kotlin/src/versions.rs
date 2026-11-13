//! Which kotlin-lsp build a start runs: a trial, the active downloaded build, or the embedded pin.
//!
//! `resolve_build` is pure. The caller reads `state.json` and the verified `cache.json` as files
//! (both are replaced by atomic renames, so no lock is taken) and passes a test for whether a build
//! is installed. A start never waits for a download and never installs a manifest build; only the
//! embedded pin installs on first need.

use std::cmp::Ordering;

use anyhow::{Result, anyhow};
use chrono::NaiveDate;

use crate::{
    manager::EXPIRED_REASON,
    manifest::ManifestEntry,
    manifest_cache::{BuildId, ServerState},
    remote_manifest::compare_versions,
};

/// The build a kotlin-lsp start runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The trial build, installed, not expired and not rejected.
    Trial(ManifestEntry),
    /// The active downloaded build, installed and newer than the pin.
    Active(ManifestEntry),
    /// The embedded pin, installed on first need.
    Pin(ManifestEntry),
    /// Every candidate expired.
    Expired,
}

impl Resolution {
    /// The entry to start, or `EXPIRED_REASON`.
    pub fn entry(&self) -> Result<&ManifestEntry> {
        match self {
            Self::Trial(entry) | Self::Active(entry) | Self::Pin(entry) => Ok(entry),
            Self::Expired => Err(anyhow!("{EXPIRED_REASON}")),
        }
    }
}

/// Chooses the kotlin-lsp build to start (rules applied in order):
///
/// 1. The trial, when it is in `cached`, installed, not expired and not rejected.
/// 2. Else the newest non-expired build of the active one (only when it is in `cached` and
///    installed) and the embedded pin. The pin wins a tie.
/// 3. Else `Expired`.
///
/// `cached` holds the entries of the verified cached manifest for this platform; a build in the
/// state that is not in it is not a candidate. `verify` ignores `expires`, so expiry is tested here
/// against `today`.
pub fn resolve_build(
    embedded: &ManifestEntry,
    cached: &[ManifestEntry],
    state: &ServerState,
    installed: impl Fn(&ManifestEntry) -> bool,
    today: NaiveDate,
) -> Resolution {
    let builds = &state.kotlin_lsp;
    let usable = |id: &Option<BuildId>| {
        id.as_ref()
            .and_then(|id| cached.iter().find(|entry| is_build(entry, id)))
            .filter(|entry| !entry.is_expired(today) && installed(entry))
    };

    let trial_rejected = builds.trial.as_ref().is_some_and(|trial| {
        builds
            .rejected
            .iter()
            .any(|rejected| same_build(rejected, trial))
    });
    if !trial_rejected && let Some(trial) = usable(&builds.trial) {
        return Resolution::Trial(trial.clone());
    }

    let pin = (!embedded.is_expired(today)).then_some(embedded);
    match (usable(&builds.active), pin) {
        (Some(active), Some(pin))
            if compare_versions(&active.version, &pin.version) == Some(Ordering::Greater) =>
        {
            Resolution::Active(active.clone())
        }
        (_, Some(pin)) => Resolution::Pin(pin.clone()),
        (Some(active), None) => Resolution::Active(active.clone()),
        (None, None) => Resolution::Expired,
    }
}

fn is_build(entry: &ManifestEntry, id: &BuildId) -> bool {
    entry.version == id.version && entry.sha256.eq_ignore_ascii_case(&id.sha256)
}

fn same_build(a: &BuildId, b: &BuildId) -> bool {
    a.version == b.version && a.sha256.eq_ignore_ascii_case(&b.sha256)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{installed::tests::entry, manifest_cache::KotlinLspBuilds};

    const PIN_SHA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OLD_SHA: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const NEW_SHA: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const TRIAL_SHA: &str = "4444444444444444444444444444444444444444444444444444444444444444";

    /// The date every resolution here sees. No test reads the wall clock or writes a calendar date
    /// for `expires`: an entry expires a number of days after `today()`.
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()
    }

    /// An entry that is still good at `today()`, for weeks.
    const FAR: i64 = 50;
    /// An entry whose last good day was the day before `today()`.
    const EXPIRED: i64 = -1;

    fn expiring(version: &str, sha256: &str, days_after_today: i64) -> ManifestEntry {
        ManifestEntry {
            expires: Some(today() + chrono::Duration::days(days_after_today)),
            ..entry(version, sha256)
        }
    }

    fn id(entry: &ManifestEntry) -> BuildId {
        BuildId {
            version: entry.version.clone(),
            sha256: entry.sha256.clone(),
        }
    }

    fn pin() -> ManifestEntry {
        expiring("262.1.0", PIN_SHA, FAR)
    }

    fn older() -> ManifestEntry {
        expiring("261.5.0", OLD_SHA, FAR)
    }

    fn newer() -> ManifestEntry {
        expiring("263.2.0", NEW_SHA, FAR)
    }

    fn trial() -> ManifestEntry {
        expiring("264.0.0", TRIAL_SHA, FAR)
    }

    fn state(active: Option<&ManifestEntry>, trial: Option<&ManifestEntry>) -> ServerState {
        ServerState {
            kotlin_lsp: KotlinLspBuilds {
                active: active.map(id),
                trial: trial.map(id),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Every build whose sha256 is in `shas` counts as installed.
    fn installed(shas: &[&str]) -> impl Fn(&ManifestEntry) -> bool {
        let shas: BTreeSet<String> = shas.iter().map(|sha| sha.to_string()).collect();
        move |entry| shas.contains(&entry.sha256)
    }

    #[test]
    fn test_no_cache_gives_the_pin() {
        let resolution = resolve_build(
            &pin(),
            &[],
            &ServerState::default(),
            installed(&[]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));

        // A state naming builds the cache does not hold changes nothing.
        let resolution = resolve_build(
            &pin(),
            &[],
            &state(Some(&newer()), Some(&trial())),
            installed(&[NEW_SHA, TRIAL_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));
    }

    #[test]
    fn test_an_installed_active_build_newer_than_the_pin_starts() {
        let resolution = resolve_build(
            &pin(),
            &[newer()],
            &state(Some(&newer()), None),
            installed(&[NEW_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Active(newer()));
    }

    #[test]
    fn test_a_pin_newer_than_the_active_build_or_tied_with_it_starts() {
        let resolution = resolve_build(
            &pin(),
            &[older()],
            &state(Some(&older()), None),
            installed(&[OLD_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));

        let tied = expiring("262.1.0", NEW_SHA, FAR);
        let resolution = resolve_build(
            &pin(),
            std::slice::from_ref(&tied),
            &state(Some(&tied), None),
            installed(&[NEW_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));
    }

    #[test]
    fn test_an_installed_trial_starts_as_a_trial() {
        let resolution = resolve_build(
            &pin(),
            &[newer(), trial()],
            &state(Some(&newer()), Some(&trial())),
            installed(&[NEW_SHA, TRIAL_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Trial(trial()));

        // A trial older than the pin still starts as the trial.
        let resolution = resolve_build(
            &pin(),
            &[older()],
            &state(None, Some(&older())),
            installed(&[OLD_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Trial(older()));
    }

    #[test]
    fn test_a_rejected_or_expired_trial_is_skipped() {
        let mut rejected = state(Some(&newer()), Some(&trial()));
        rejected.kotlin_lsp.rejected.push(id(&trial()));
        let resolution = resolve_build(
            &pin(),
            &[newer(), trial()],
            &rejected,
            installed(&[NEW_SHA, TRIAL_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Active(newer()));

        let expired_trial = expiring("264.0.0", TRIAL_SHA, EXPIRED);
        let resolution = resolve_build(
            &pin(),
            std::slice::from_ref(&expired_trial),
            &state(None, Some(&expired_trial)),
            installed(&[TRIAL_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));
    }

    /// `rejected` names a build by version and digest: the same version rebuilt with another digest
    /// is a different build and may run as the trial, while the rejected one never does.
    #[test]
    fn test_a_trial_of_the_same_version_with_a_new_digest_is_eligible_again() {
        let rebuilt = expiring("264.0.0", OLD_SHA, FAR);
        let mut state = state(Some(&newer()), Some(&rebuilt));
        state.kotlin_lsp.rejected.push(id(&trial()));
        let resolution = resolve_build(
            &pin(),
            &[newer(), rebuilt.clone()],
            &state,
            installed(&[NEW_SHA, OLD_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Trial(rebuilt));

        // The rejected digest itself, listed again, is still skipped.
        state.kotlin_lsp.trial = Some(id(&trial()));
        let resolution = resolve_build(
            &pin(),
            &[newer(), trial()],
            &state,
            installed(&[NEW_SHA, TRIAL_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Active(newer()));
    }

    #[test]
    fn test_an_expired_active_build_falls_back_to_the_pin() {
        let expired = expiring("263.2.0", NEW_SHA, EXPIRED);
        let resolution = resolve_build(
            &pin(),
            std::slice::from_ref(&expired),
            &state(Some(&expired), None),
            installed(&[NEW_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));
    }

    #[test]
    fn test_an_active_build_starts_when_the_pin_expired() {
        let expired_pin = expiring("262.1.0", PIN_SHA, EXPIRED);
        let resolution = resolve_build(
            &expired_pin,
            &[older()],
            &state(Some(&older()), None),
            installed(&[OLD_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Active(older()));
    }

    #[test]
    fn test_everything_expired_gives_the_expired_reason() {
        let expired_pin = expiring("262.1.0", PIN_SHA, EXPIRED);
        let expired_active = expiring("263.2.0", NEW_SHA, EXPIRED);
        let resolution = resolve_build(
            &expired_pin,
            std::slice::from_ref(&expired_active),
            &state(Some(&expired_active), None),
            installed(&[PIN_SHA, NEW_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Expired);
        assert_eq!(resolution.entry().unwrap_err().to_string(), EXPIRED_REASON);
        assert_eq!(Resolution::Pin(pin()).entry().unwrap().sha256, pin().sha256);
    }

    #[test]
    fn test_a_cached_build_that_is_not_installed_is_never_chosen() {
        let resolution = resolve_build(
            &pin(),
            &[newer(), trial()],
            &state(Some(&newer()), Some(&trial())),
            installed(&[]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));

        let expired_pin = expiring("262.1.0", PIN_SHA, EXPIRED);
        let resolution = resolve_build(
            &expired_pin,
            &[newer()],
            &state(Some(&newer()), None),
            installed(&[]),
            today(),
        );
        assert_eq!(resolution, Resolution::Expired);
    }

    #[test]
    fn test_a_state_build_with_another_digest_is_not_the_cached_one() {
        let mut other = id(&newer());
        other.sha256 = OLD_SHA.into();
        let mut state = state(None, None);
        state.kotlin_lsp.active = Some(other);
        let resolution = resolve_build(
            &pin(),
            &[newer()],
            &state,
            installed(&[NEW_SHA, OLD_SHA]),
            today(),
        );
        assert_eq!(resolution, Resolution::Pin(pin()));
    }
}
