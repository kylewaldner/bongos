use std::path::PathBuf;

use bongos_project_repo::{AddExistingSource, RestoreOutcome};
use tempfile::TempDir;

use crate::restore_commands::{add_source, restore_message};

fn folder(slug: &str) -> PathBuf {
    PathBuf::from("/projects").join(slug)
}

#[test]
fn add_existing_tells_a_folder_from_a_remote_address() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("invoicing");
    std::fs::create_dir_all(project.join("repository").join(".git")).unwrap();
    let repository = root.path().join("bare-checkout");
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    let template = "{folder}/repository";

    for path in [project, repository] {
        assert_eq!(
            add_source(path.to_str().unwrap(), template, folder),
            AddExistingSource::Path { path }
        );
    }
    let plain = root.path().join("plain");
    for (address, slug) in [
        ("git@host:team/invoicing.git", "invoicing"),
        ("/backups/billing.git", "billing"),
        ("ssh://host/srv/Ledger Book/", "ledger-book"),
        (plain.to_str().unwrap(), "plain"),
    ] {
        assert_eq!(
            add_source(address, template, folder),
            AddExistingSource::Remote {
                address: address.to_string(),
                folder: folder(slug),
            },
            "{address}"
        );
    }
}

#[test]
fn a_refused_restore_says_what_runs() {
    let message = restore_message(&RestoreOutcome::Refused {
        activity: vec!["lanes: lane 1 runs build".to_string()],
    });
    assert_eq!(
        message,
        "Not restored: work is running (lanes: lane 1 runs build); stop it at a job boundary first"
    );
}
