//! The author a `lanes:` command is recorded as, by whom it came from, and the last run of a lane
//! tree a job's reset is told of.

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

#[test]
fn a_start_takes_its_bound_job_count_and_unattended_override_from_the_command_line() {
    use bongos_lanes_model::{PickBound, TaskId};

    use crate::host::start_request;

    // What `bongos lanes start --until l2 --max-jobs 3 --unattended off` sends.
    let payload: LanesPayload = serde_json::from_value(serde_json::json!({
        "until": "l2",
        "max_jobs": 3,
        "within": null,
        "unattended": "off",
    }))
    .unwrap();
    let start = start_request(&payload, Author::System).unwrap();
    assert_eq!(start.bound, Some(PickBound::Until(TaskId::new("l2"))));
    assert_eq!(start.max_jobs, Some(3));
    assert_eq!(start.unattended, Some(false));

    // A window's start sends the override as a bool; nothing set leaves everything unset.
    let payload: LanesPayload =
        serde_json::from_value(serde_json::json!({ "within": "g1", "unattended": true })).unwrap();
    let start = start_request(&payload, Author::System).unwrap();
    assert_eq!(start.bound, Some(PickBound::Within(TaskId::new("g1"))));
    assert_eq!(start.unattended, Some(true));
    let start = start_request(&LanesPayload::default(), Author::System).unwrap();
    assert_eq!(
        (start.bound, start.max_jobs, start.unattended),
        (None, None, None)
    );

    // Both bounds, no jobs, or an override that is neither on nor off are refused.
    let both = LanesPayload {
        within: Some("g1".to_string()),
        until: Some("l2".to_string()),
        ..LanesPayload::default()
    };
    assert!(start_request(&both, Author::System).is_err());
    let none = LanesPayload {
        max_jobs: Some(0),
        ..LanesPayload::default()
    };
    assert!(start_request(&none, Author::System).is_err());
    assert!(
        serde_json::from_value::<LanesPayload>(serde_json::json!({ "unattended": "maybe" }))
            .is_err()
    );
}

#[test]
fn a_run_still_using_the_lane_tree_makes_the_job_wait_and_an_ended_one_is_its_last_run() {
    use bongos_lanes_model::WaitReason;
    use bongos_workflow::RunStatus;

    use crate::host::last_run_of;
    use crate::lane_tree::LastEnd;

    const RUN: &str = "01890a5d-ac96-774b-bcce-b30209000001";
    let of = |status| last_run_of(RUN.to_string(), "task/x-run-09000001-1".to_string(), status);
    for status in [
        RunStatus::Queued,
        RunStatus::Running,
        RunStatus::Pausing,
        RunStatus::Paused,
        RunStatus::WaitingOnYou,
    ] {
        match of(status) {
            Err(WaitReason::Environment(text)) => {
                assert!(text.contains("still uses the lane tree"), "{text}");
                assert!(text.contains(status.label()), "{text}");
            }
            other => panic!("{status:?}: {other:?}"),
        }
    }
    for (status, end) in [
        (RunStatus::Halted, LastEnd::Halted),
        (RunStatus::Failed, LastEnd::Failed),
        (RunStatus::Cancelled, LastEnd::Cancelled),
        (RunStatus::Succeeded, LastEnd::Succeeded),
    ] {
        let last = of(status).unwrap();
        assert_eq!((last.run_id.as_str(), last.end), (RUN, end));
        assert_eq!(last.run_branch, "task/x-run-09000001-1");
    }
}

#[gpui::test]
fn with_no_run_engine_the_last_run_is_unknown(cx: &mut gpui::TestAppContext) {
    use bongos_lanes_model::LaneId;

    use crate::host::last_lane_run;

    let last = cx.update(|cx| last_lane_run(&LaneId::new("build", 1), "run", cx));
    assert_eq!(last, Ok(None));
}
