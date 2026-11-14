use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work_model::TodoState;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::backup::{DEFAULT_STATE_BRANCH, SnapshotReason};
use crate::snapshot_tests::{change_state, project, start, writer};
use crate::test_support::{git, main_commit, outside_commit, run_git, todo};
use crate::{
    AddExistingRequest, AddExistingSource, AddedProject, LayoutTemplates, ProjectRepo,
    ProjectValueOverrides, RepositoryLocations, RestoreOutcome, RestoreSource,
};

/// A bare repository at `root/name`.
fn bare(root: &TempDir, name: &str) -> PathBuf {
    let path = root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    block_on(git().run(&path, &["init", "-q", "--bare"])).unwrap();
    path
}

/// The fixture project with its master and a snapshot (task `l1`'s first todo done) pushed to
/// a bare remote. Returns the project and the remote.
fn pushed(root: &TempDir) -> (ProjectRepo, PathBuf) {
    let remote = bare(root, "remote.git");
    let repo = project(root);
    let mut location = repo.location().clone();
    location.remote = Some(remote.to_string_lossy().into_owned());
    let repo =
        ProjectRepo::new(location, repo.git().clone()).with_state(repo.state().unwrap().clone());
    repo.set_backup_options(crate::snapshot_tests::test_options());
    repo.use_exporters(Vec::new());
    repo.use_value_overrides(Arc::new(ProjectValueOverrides::new()));
    run_git(&repo, &["push", "-q", remote.to_str().unwrap(), "main"]);
    change_state(&repo, TodoState::Done);
    block_on(repo.snapshot_now(SnapshotReason::Manual, start())).unwrap();
    assert!(block_on(repo.push_now(start())).unwrap());
    (repo, remote)
}

fn request(source: AddExistingSource, remote: Option<String>) -> AddExistingRequest {
    AddExistingRequest {
        source,
        layout: LayoutTemplates::default(),
        state_branch: DEFAULT_STATE_BRANCH.to_string(),
        remote,
        checks: None,
        timeout: None,
    }
}

fn add(
    request: AddExistingRequest,
    locations: &mut RepositoryLocations,
) -> anyhow::Result<AddedProject> {
    block_on(ProjectRepo::add_existing(request, locations, || Ok(git())))
}

fn locations(root: &TempDir) -> RepositoryLocations {
    RepositoryLocations::load(root.path().join("machine").join("locations.toml")).unwrap()
}

fn configured(added: &AddedProject) -> &ProjectRepo {
    added
        .repo
        .set_backup_options(crate::snapshot_tests::test_options());
    added.repo.use_exporters(Vec::new());
    added
        .repo
        .use_value_overrides(Arc::new(ProjectValueOverrides::new()));
    added.repo.use_activity_sources(Vec::new());
    &added.repo
}

#[test]
fn a_project_added_from_its_remote_is_verified_and_offers_restore() {
    let root = TempDir::new().unwrap();
    let (original, remote) = pushed(&root);
    let mut known = locations(&root);
    let folder = root.path().join("new-machine").join("invoicing");
    let added = add(
        request(
            AddExistingSource::Remote {
                address: remote.to_string_lossy().into_owned(),
                folder: folder.clone(),
            },
            None,
        ),
        &mut known,
    )
    .unwrap();

    let location = added.repo.location().clone();
    assert_eq!(location.id, "project-1");
    assert_eq!(location.folder, folder);
    assert_eq!(location.remote, Some(remote.to_string_lossy().into_owned()));
    assert_eq!(known.location("project-1"), Some(&location));
    // The record is on disk.
    assert_eq!(locations(&root).location("project-1"), Some(&location));
    assert_eq!(added.verified, Some(main_commit(&original)));
    assert!(added.problems.is_empty(), "{:?}", added.problems);
    assert!(added.has_snapshot);
    for part in [&location.working_copies, &location.state, &location.files] {
        assert!(part.is_dir(), "{}", part.display());
    }
    let project_file = location.repository.join("definitions").join("project.toml");
    assert!(
        std::fs::metadata(project_file)
            .unwrap()
            .permissions()
            .readonly()
    );

    // Restoring from the newest snapshot brings back the state of the first machine.
    let repo = configured(&added);
    assert_eq!(writer(repo).read(|read| read.task("l1")).unwrap(), None);
    let outcome = block_on(repo.restore(&RestoreSource::Newest, start())).unwrap();
    assert!(
        matches!(outcome, RestoreOutcome::Restored { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        writer(repo)
            .read(|read| read.task("l1"))
            .unwrap()
            .unwrap()
            .todos,
        vec![todo("t1", TodoState::Done)]
    );
}

#[test]
fn a_project_added_from_a_folder_fetches_the_remote_state_branch() {
    let root = TempDir::new().unwrap();
    let (_original, remote) = pushed(&root);
    // Another machine's copy of master alone, without its state branch.
    let folder = root.path().join("copy");
    let repository = folder.join("repository");
    std::fs::create_dir_all(&folder).unwrap();
    block_on(git().run(
        &folder,
        &[
            "clone",
            "-q",
            "--single-branch",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            repository.to_str().unwrap(),
        ],
    ))
    .unwrap();

    // The repository itself, or its project folder: both are found.
    for path in [repository.clone(), folder.clone()] {
        let mut known = locations(&root);
        let added = add(
            request(
                AddExistingSource::Path { path },
                Some(remote.to_string_lossy().into_owned()),
            ),
            &mut known,
        )
        .unwrap();
        assert_eq!(added.repo.location().repository, repository);
        assert_eq!(added.repo.location().folder, folder);
        assert!(added.has_snapshot);
        assert!(added.notes.is_empty(), "{:?}", added.notes);
        let candidates = block_on(configured(&added).restore_candidates()).unwrap();
        assert!(candidates.newest.is_some(), "{candidates:?}");
    }
}

#[test]
fn a_failed_add_leaves_nothing_behind() {
    let root = TempDir::new().unwrap();
    let mut known = locations(&root);
    let folder = root.path().join("new");
    let missing = root.path().join("no-such-remote.git");
    let error = add(
        request(
            AddExistingSource::Remote {
                address: missing.to_string_lossy().into_owned(),
                folder: folder.clone(),
            },
            None,
        ),
        &mut known,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("cloning"), "{error:#}");
    assert!(!folder.exists());
    assert!(known.locations().is_empty());

    // An address that would be read as an option, or the ext transport, is never run.
    for address in ["--upload-pack=touch x", "ext::sh -c touch% x"] {
        let error = add(
            request(
                AddExistingSource::Remote {
                    address: address.to_string(),
                    folder: folder.clone(),
                },
                None,
            ),
            &mut known,
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("the address"), "{error:#}");
        assert!(!folder.exists());
    }

    // A repository whose project.toml does not parse.
    let broken = bare(&root, "broken.git");
    let (original, _) = pushed(&root);
    let commit = block_on(outside_commit(
        original.git().clone(),
        original.location().repository.clone(),
        vec![(
            "definitions/project.toml".to_string(),
            "schema = [[[".to_string(),
        )],
        "break it".to_string(),
    ));
    run_git(
        &original,
        &[
            "push",
            "-q",
            broken.to_str().unwrap(),
            &format!("{commit}:refs/heads/main"),
        ],
    );
    let error = add(
        request(
            AddExistingSource::Remote {
                address: broken.to_string_lossy().into_owned(),
                folder: folder.clone(),
            },
            None,
        ),
        &mut known,
    )
    .unwrap_err();
    assert!(error.to_string().contains("does not parse"), "{error:#}");
    assert!(!folder.exists());
    assert!(known.locations().is_empty());

    // A folder that is no project.
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let error = add(
        request(
            AddExistingSource::Path {
                path: plain.clone(),
            },
            None,
        ),
        &mut known,
    )
    .unwrap_err();
    assert!(error.to_string().contains("neither a project folder"));
    assert_eq!(std::fs::read_dir(&plain).unwrap().count(), 0);
}

#[test]
fn a_master_that_fails_the_checks_is_not_verified() {
    let root = TempDir::new().unwrap();
    let (original, remote) = pushed(&root);
    let commit = block_on(outside_commit(
        original.git().clone(),
        original.location().repository.clone(),
        vec![(
            "definitions/tasks/broken/task.toml".to_string(),
            "this is not toml = [".to_string(),
        )],
        "a broken task".to_string(),
    ));
    run_git(
        &original,
        &[
            "push",
            "-q",
            "-f",
            remote.to_str().unwrap(),
            &format!("{commit}:refs/heads/main"),
        ],
    );
    let mut known = locations(&root);
    let added = add(
        request(
            AddExistingSource::Remote {
                address: remote.to_string_lossy().into_owned(),
                folder: root.path().join("second"),
            },
            None,
        ),
        &mut known,
    )
    .unwrap();
    assert_eq!(added.verified, None);
    assert!(!added.problems.is_empty());
    assert_eq!(added.repo.verified_commit().unwrap(), None);
}

#[test]
fn a_project_already_on_this_machine_elsewhere_is_refused() {
    let root = TempDir::new().unwrap();
    let (_original, remote) = pushed(&root);
    let mut known = locations(&root);
    let address = remote.to_string_lossy().into_owned();
    let first = add(
        request(
            AddExistingSource::Remote {
                address: address.clone(),
                folder: root.path().join("one"),
            },
            None,
        ),
        &mut known,
    )
    .unwrap();
    drop(first);
    let second = root.path().join("two");
    let error = add(
        request(
            AddExistingSource::Remote {
                address,
                folder: second.clone(),
            },
            None,
        ),
        &mut known,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("already on this machine"),
        "{error:#}"
    );
    assert!(!second.exists());
    assert_eq!(known.locations().len(), 1);
    assert!(Path::new(&known.locations()[0].folder).ends_with("one"));
}
