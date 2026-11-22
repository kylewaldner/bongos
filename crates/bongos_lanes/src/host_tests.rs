//! The author a `lanes:` command is recorded as, by whom it came from.

use bongos_lanes_model::Author;
use bongos_project_repo::{ClientKind, PeerIdentity, peer_identity};

use crate::host::{LanesPayload, author_of};

fn peer(kind: ClientKind) -> PeerIdentity {
    let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    peer_identity(&ours, kind).unwrap()
}

fn kim() -> LanesPayload {
    LanesPayload {
        person: Some("kim".to_string()),
        ..LanesPayload::default()
    }
}

#[test]
fn a_rerouted_person_claim_names_the_author_only_from_a_window_of_this_executable() {
    assert_eq!(
        author_of(&kim(), &peer(ClientKind::Window)),
        Author::Person("kim".to_string())
    );
    assert_eq!(author_of(&kim(), &peer(ClientKind::Cli)), Author::System);
    assert_eq!(
        author_of(&LanesPayload::default(), &peer(ClientKind::Window)),
        Author::System
    );
}
