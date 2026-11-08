//! What the IntelliJ server needs before it starts, beyond an install (decisions D9 and D10): the
//! user's acceptance of the bundle's EULA, remembered per EULA hash, the `--eula` argument built
//! from it, and the `license status` pre-check. bongos never accepts the agreement for the user:
//! `accept_eula` is called by the user's own action only.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures::FutureExt as _;
use lsp::LanguageServerBinary;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::resolve::{commit_staged, stage};

/// The agreement at the root of the IntelliJ bundle.
pub const EULA_FILE: &str = "EULA.txt";

/// The `Failed` text of an IntelliJ start whose agreement the user has not accepted.
pub const EULA_NOT_ACCEPTED_REASON: &str = "IntelliJ EULA not accepted";

/// Where acceptances are kept, under the state directory.
const STATE_FILE: &str = "kotlin-eula.json";
const STATE_VERSION: u32 = 1;

/// How long `license status` may take before the check counts as failed.
pub const LICENSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The exit code of `license status` when the server has no valid license (step 1: exit 2).
pub const UNLICENSED_EXIT_CODE: i32 = 2;

/// The first 16 hex characters of the SHA-256 of the agreement, the value `--eula=` takes.
pub fn eula_hash(text: &[u8]) -> String {
    let digest = format!("{:x}", Sha256::digest(text));
    digest[..16].to_string()
}

/// The hash of the `EULA.txt` in an installed bundle.
pub fn bundle_eula_hash(bundle_dir: &Path) -> Result<String> {
    let path = bundle_dir.join(EULA_FILE);
    let text = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(eula_hash(&text))
}

/// The argument that tells the server the agreement with this hash is accepted.
pub fn eula_argument(hash: &str) -> OsString {
    OsString::from(format!("--eula={hash}"))
}

#[derive(Serialize, Deserialize)]
struct EulaFile {
    version: u32,
    /// Accepted hash to the RFC 3339 time of acceptance.
    accepted: BTreeMap<String, String>,
}

impl Default for EulaFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            accepted: BTreeMap::new(),
        }
    }
}

fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_FILE)
}

/// The acceptances on disk. A missing, unreadable or unsupported file is no acceptance at all:
/// the user is asked again, never assumed to have agreed.
fn load(state_dir: &Path) -> EulaFile {
    fs::read_to_string(state_path(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<EulaFile>(&text).ok())
        .filter(|file| file.version == STATE_VERSION)
        .unwrap_or_default()
}

/// Whether the user accepted the agreement with this hash. A new bundle with a new agreement has
/// a new hash and asks again.
pub fn eula_accepted(state_dir: &Path, hash: &str) -> bool {
    load(state_dir).accepted.contains_key(hash)
}

/// Records the user's acceptance of the agreement with this hash.
pub fn accept_eula(state_dir: &Path, hash: &str) -> Result<()> {
    let mut file = load(state_dir);
    file.accepted
        .insert(hash.to_string(), chrono::Utc::now().to_rfc3339());
    fs::create_dir_all(state_dir)?;
    let path = state_path(state_dir);
    let staged = stage(&path, &serde_json::to_vec_pretty(&file)?)?;
    commit_staged(&staged, &path)
}

/// The result of `license status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LicenseOutcome {
    /// Exit 0.
    Licensed,
    /// The pinned "no valid license" exit code.
    Unlicensed,
    /// Any other exit: the license server or account could not be reached, or the command failed.
    Offline,
    /// `license status` did not answer in time.
    TimedOut,
}

impl LicenseOutcome {
    /// What an exit code of `license status` means; `None` is a process the system killed.
    pub fn from_exit_code(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Licensed,
            Some(UNLICENSED_EXIT_CODE) => Self::Unlicensed,
            _ => Self::Offline,
        }
    }

    /// The status text of a check that does not allow the start; `None` when licensed.
    pub fn failure_reason(self) -> Option<&'static str> {
        match self {
            Self::Licensed => None,
            Self::Unlicensed => Some("IntelliJ unlicensed"),
            Self::Offline | Self::TimedOut => Some("license check failed (offline?)"),
        }
    }
}

/// Runs `<launcher> license status` with the launcher's environment. `timeout` is a future that
/// completes when time is up (`executor.timer(LICENSE_TIMEOUT)` in the app); the process is killed
/// when it loses the race, and when the check is dropped. Only a command that cannot start is an
/// error.
pub async fn license_check(
    binary: &LanguageServerBinary,
    timeout: impl Future<Output = ()>,
) -> Result<LicenseOutcome> {
    let mut command = util::command::new_command(&binary.path);
    command
        .args(["license", "status"])
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::null())
        .stderr(util::command::Stdio::null())
        .kill_on_drop(true);
    if let Some(env) = &binary.env {
        command.envs(env);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("running {} license status", binary.path.display()))?;
    let status = child.status().fuse();
    let timeout = timeout.fuse();
    futures::pin_mut!(status, timeout);
    futures::select_biased! {
        status = status => {
            let status = status.context("waiting for license status")?;
            Ok(LicenseOutcome::from_exit_code(status.code()))
        }
        () = timeout => Ok(LicenseOutcome::TimedOut),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use futures::executor::block_on;

    use super::*;

    #[test]
    fn test_eula_hash_is_sixteen_hex_characters_of_the_sha256() {
        // SHA-256 of the empty string starts e3b0c44298fc1c14.
        assert_eq!(eula_hash(b""), "e3b0c44298fc1c14");
        assert_eq!(eula_hash(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(eula_argument("ba7816bf8f01cfea"), "--eula=ba7816bf8f01cfea");
    }

    #[test]
    fn test_hash_follows_the_bundles_eula_text() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join(EULA_FILE), "agreement version 1").unwrap();
        fs::write(second.path().join(EULA_FILE), "agreement version 2").unwrap();
        let (one, two) = (
            bundle_eula_hash(first.path()).unwrap(),
            bundle_eula_hash(second.path()).unwrap(),
        );
        assert_eq!(one, eula_hash(b"agreement version 1"));
        assert_ne!(one, two, "a changed agreement must change the hash");
        // A bundle without the file has no hash.
        let empty = tempfile::tempdir().unwrap();
        assert!(bundle_eula_hash(empty.path()).is_err());
    }

    #[test]
    fn test_acceptance_is_stored_per_hash() {
        let state = tempfile::tempdir().unwrap();
        let state_dir = state.path().join("nested");
        assert!(!eula_accepted(&state_dir, "aaaaaaaaaaaaaaaa"));

        accept_eula(&state_dir, "aaaaaaaaaaaaaaaa").unwrap();
        assert!(eula_accepted(&state_dir, "aaaaaaaaaaaaaaaa"));
        assert!(
            !eula_accepted(&state_dir, "bbbbbbbbbbbbbbbb"),
            "a new agreement asks again"
        );

        accept_eula(&state_dir, "bbbbbbbbbbbbbbbb").unwrap();
        assert!(eula_accepted(&state_dir, "aaaaaaaaaaaaaaaa"));
        assert!(eula_accepted(&state_dir, "bbbbbbbbbbbbbbbb"));
        assert!(!state_dir.join("kotlin-eula.json.tmp").exists());
    }

    #[test]
    fn test_a_corrupt_or_foreign_state_file_accepts_nothing() {
        let state = tempfile::tempdir().unwrap();
        fs::write(state.path().join(STATE_FILE), "not json").unwrap();
        assert!(!eula_accepted(state.path(), "aaaaaaaaaaaaaaaa"));
        fs::write(
            state.path().join(STATE_FILE),
            r#"{ "version": 9, "accepted": { "aaaaaaaaaaaaaaaa": "x" } }"#,
        )
        .unwrap();
        assert!(!eula_accepted(state.path(), "aaaaaaaaaaaaaaaa"));
        // Accepting again repairs the file.
        accept_eula(state.path(), "aaaaaaaaaaaaaaaa").unwrap();
        assert!(eula_accepted(state.path(), "aaaaaaaaaaaaaaaa"));
    }

    fn script(dir: &Path, body: &str) -> LanguageServerBinary {
        let path = dir.join("launcher");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        LanguageServerBinary {
            path,
            arguments: Vec::new(),
            env: None,
        }
    }

    fn check(binary: &LanguageServerBinary) -> LicenseOutcome {
        block_on(license_check(binary, std::future::pending())).unwrap()
    }

    #[test]
    fn test_license_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        // The script sees `license status` as its arguments.
        let binary = script(
            dir.path(),
            r#"[ "$1 $2" = "license status" ] || exit 99; exit "${FAKE_EXIT:-0}""#,
        );
        let with_exit = |code: &str| LanguageServerBinary {
            env: Some(
                [("FAKE_EXIT".to_string(), code.to_string())]
                    .into_iter()
                    .collect(),
            ),
            ..binary.clone()
        };
        assert_eq!(check(&with_exit("0")), LicenseOutcome::Licensed);
        assert_eq!(check(&with_exit("2")), LicenseOutcome::Unlicensed);
        assert_eq!(check(&with_exit("3")), LicenseOutcome::Offline);
        assert_eq!(check(&with_exit("1")), LicenseOutcome::Offline);
    }

    #[test]
    fn test_a_license_check_that_does_not_answer_times_out_and_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let binary = script(
            dir.path(),
            &format!("echo $$ > '{}'\nexec sleep 60", pid_file.display()),
        );
        // The deadline passes once the script has written its pid, so the process is running.
        let started = async {
            while !fs::read_to_string(&pid_file).is_ok_and(|pid| pid.ends_with('\n')) {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let outcome = block_on(license_check(&binary, started)).unwrap();
        assert_eq!(outcome, LicenseOutcome::TimedOut);
        assert_eq!(
            outcome.failure_reason(),
            Some("license check failed (offline?)")
        );
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).unwrap();
            let process = PathBuf::from("/proc").join(pid.trim());
            for _ in 0..500 {
                if !process.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the timed out license check is still running");
        }
    }

    #[test]
    fn test_a_launcher_that_cannot_start_is_an_error() {
        let binary = LanguageServerBinary {
            path: PathBuf::from("/nonexistent/intellij-server"),
            arguments: Vec::new(),
            env: None,
        };
        assert!(block_on(license_check(&binary, std::future::pending())).is_err());
    }

    #[test]
    fn test_failure_reasons() {
        assert_eq!(LicenseOutcome::Licensed.failure_reason(), None);
        assert_eq!(
            LicenseOutcome::Unlicensed.failure_reason(),
            Some("IntelliJ unlicensed")
        );
        assert_eq!(
            LicenseOutcome::Offline.failure_reason(),
            Some("license check failed (offline?)")
        );
    }
}
