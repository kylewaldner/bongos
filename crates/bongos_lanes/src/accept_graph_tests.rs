//! `lanes: accept graph` over a real project repository in a temporary directory: the stamp of
//! master lands as the person's own change, once.

use std::sync::Arc;

use bongos_project_repo::{
    ClientKind, Git, Identity, LandingEvent, Location, MAIN_BRANCH, PeerIdentity, PersonClaim,
    ProjectRepo, StateApplyHooks, StateHandle, StateStore, peer_identity,
};
use bongos_project_repo_model::PersonAtKeyboard;
use bongos_work_model::commands::Context as WorkContext;
use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use futures::executor::block_on;
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::accept_graph::{
    Accepted, accept_graph_for_peer, accept_graph_now, read_stamp, stamp_at, stamp_path,
};

const TABLE: &str =
    "schema = 1\n[[kind]]\nname = \"build\"\ncount = 2\n[[pin]]\ntask = \"l1\"\nkind = \"build\"\n";

fn task_toml(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n"
    )
}

/// A project repository inside `root`: the top group `g1` with the leaves `l1` and `l2` and a lane
/// table, committed on `main`, with its state store open for writing.
fn project(root: &TempDir) -> ProjectRepo {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    };
    let definitions = location.repository.join("definitions");
    for (path, text) in [
        (
            "project.toml",
            "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n".to_string(),
        ),
        ("task.toml", task_toml("g1", "Invoicing")),
        ("plan.md", String::new()),
        ("tasks/backfill/task.toml", task_toml("l1", "Backfill")),
        ("tasks/export/task.toml", task_toml("l2", "Export")),
        ("lanes/lanes.toml", TABLE.to_string()),
    ] {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    block_on(async {
        git.init_repo(&location.repository, MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer =
        Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap());
    ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer))
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn keyboard() -> WorkContext {
    WorkContext::at_keyboard(PersonAtKeyboard::for_test())
}

fn main_commit(repo: &ProjectRepo) -> String {
    block_on(
        repo.git()
            .run(&repo.location().repository, &["rev-parse", MAIN_BRANCH]),
    )
    .unwrap()
}

// With no stamp.toml the check is off; accept graph lands the stamp of master as the
// person's own change, and the stamp at the new master matches its graph; a second accept makes
// nothing.
#[gpui::test]
async fn accept_graph_lands_the_stamp_of_master_once(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    cx.update(|cx| {
        crate::definitions::register(cx);
        repo.use_registered_checks(cx);
    });
    let master = main_commit(&repo);
    assert_eq!(read_stamp(&repo, &master).await.unwrap(), None);

    let accepted = accept_graph_now(&repo, keyboard(), "work/", now())
        .await
        .unwrap();

    let Accepted::Landed { commit, digest } = accepted else {
        panic!("expected a landing: {accepted:?}");
    };
    assert_eq!(main_commit(&repo), commit);
    let recorded = read_stamp(&repo, &commit).await.unwrap().unwrap();
    assert_eq!(recorded.digest, digest);
    assert_eq!(recorded, stamp_at(&repo, &commit).await.unwrap());
    assert_eq!(recorded, stamp_at(&repo, &master).await.unwrap());
    assert_eq!(
        recorded
            .tasks
            .keys()
            .map(|task| task.0.as_str())
            .collect::<Vec<_>>(),
        ["l1", "l2"]
    );
    let changed = block_on(repo.git().run(
        &repo.location().repository,
        &["diff", "--name-only", &master, &commit],
    ))
    .unwrap();
    assert_eq!(changed, stamp_path());

    let again = accept_graph_now(&repo, keyboard(), "work/", now())
        .await
        .unwrap();
    assert_eq!(again, Accepted::Unchanged { digest });
    assert_eq!(main_commit(&repo), commit);
}

// The lanes host listens to the project repository: the new verified commit an accept lands is
// heard, so a running dispatcher reads its table and stamp there.
#[gpui::test]
async fn the_lanes_host_hears_the_verified_commit_an_accept_lands(cx: &mut TestAppContext) {
    use crate::dispatcher::LanesHost as _;

    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    cx.update(|cx| {
        crate::definitions::register(cx);
        repo.use_registered_checks(cx);
    });
    let host = crate::host::ProjectLanesHost::new(repo.clone());
    let mut events = host.repo_events().expect("a project repository has events");

    let accepted = accept_graph_now(&repo, keyboard(), "work/", now())
        .await
        .unwrap();
    let Accepted::Landed { commit, .. } = accepted else {
        panic!("expected a landing: {accepted:?}");
    };
    let heard = loop {
        match events.next().await.expect("the repository's events go on") {
            bongos_project_repo::RepoEvent::VerifiedCommitChanged { commit } => break commit,
            _ => continue,
        }
    };
    assert_eq!(heard, commit);
}

fn peer(kind: ClientKind) -> PeerIdentity {
    let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    peer_identity(&ours, kind).unwrap()
}

fn claim() -> Option<PersonClaim> {
    Some(PersonClaim {
        name: "kim".to_string(),
    })
}

// The writer lands the stamp as the person only for a window of its own executable that sent a
// claim; a command-line peer's claim is ignored, so its change is made but never approved.
#[gpui::test]
async fn a_rerouted_accept_graph_lands_for_a_window_peers_claim_and_waits_for_a_cli_peers(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    cx.update(|cx| {
        crate::definitions::register(cx);
        repo.use_registered_checks(cx);
    });
    let master = main_commit(&repo);

    let Accepted::Landing { change_id, .. } =
        accept_graph_for_peer(&repo, claim(), &peer(ClientKind::Cli), "work/", now())
            .await
            .unwrap()
    else {
        panic!("the change is made and its landing runs on");
    };
    // No approval row was written for the CLI peer's claim, and master did not move.
    assert!(repo.approvals(&change_id).unwrap().is_empty());
    assert_eq!(main_commit(&repo), master);

    let mut events = repo.landing_events();
    let Accepted::Landing { change_id, .. } =
        accept_graph_for_peer(&repo, claim(), &peer(ClientKind::Window), "work/", now())
            .await
            .unwrap()
    else {
        panic!("the change is made and its landing runs on");
    };
    // The landing runs on in the writer; its end arrives as an event.
    let commit = loop {
        match events.next().await {
            Some((id, LandingEvent::Landed { commit })) if id == change_id => break commit,
            Some(_) => {}
            None => panic!("the landing events ended"),
        }
    };
    assert_eq!(main_commit(&repo), commit);
    assert!(!repo.approvals(&change_id).unwrap().is_empty());
}
