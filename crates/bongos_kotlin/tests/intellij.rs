//! The license pre-check against the fake server as a real process: `license status` exits per
//! `BONGOS_FAKE_LICENSE`, and the check maps the exit code to an outcome.
#![cfg(unix)]

use bongos_kotlin::{LicenseOutcome, license_check};
use lsp::LanguageServerBinary;

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_bongos-fake-kotlin-server");

fn outcome(license: &str) -> LicenseOutcome {
    let binary = LanguageServerBinary {
        path: FAKE_SERVER.into(),
        arguments: Vec::new(),
        env: Some(
            [("BONGOS_FAKE_LICENSE".to_string(), license.to_string())]
                .into_iter()
                .collect(),
        ),
    };
    futures::executor::block_on(license_check(&binary, std::future::pending())).unwrap()
}

#[test]
fn test_the_fake_servers_license_status_maps_to_outcomes() {
    assert_eq!(outcome("licensed"), LicenseOutcome::Licensed);
    assert_eq!(outcome("unlicensed"), LicenseOutcome::Unlicensed);
    assert_eq!(outcome("offline"), LicenseOutcome::Offline);
}
