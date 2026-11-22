//! Tests of done owners: the clearing transaction over a real state store, the dispatcher's
//! passes that run it, the lending of a done owner's tree over real git while checks are owed,
//! and the lanes' trees registered for the checks.

use std::collections::BTreeMap;
use std::sync::Arc;

use bongos_checks::{
    DONE_NAMESPACE, DoneRecord, OwedMarker, SecondTree, TaskTreesRegistry, owing_tasks, put_done,
    put_marker,
};
use bongos_lanes_model::{
    LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState, PickGraph, PickTask, TaskId,
};
use bongos_project_repo::{StateEvent, StateWriter};
use futures::StreamExt as _;
use gpui::TestAppContext;
use settings::SettingsStore;
use tempfile::TempDir;

use crate::dispatcher_tests::{
    Harness, builds, finish, harness, harness_in, history_of, leaf, start, start_request,
};
use crate::done_owners::{CLEARED_NO_CHECKS, clear_done_owners_in};
use crate::job_trees::CurrentJobTrees;
use crate::lane_tree_tests::{Fixture, RUN_1};
use crate::runs::LaneRuns;
use crate::store::{LANES_NAMESPACE, put_lane, put_owner, read_state};
use crate::task_trees::{LaneTaskTrees, no_top_level};
use crate::task_trees_tests::{TABLE, answer, build, owner, path_of, process, setup, start_job};
use crate::test_support::{SINCE, writer};

fn task(id: &str) -> TaskId {
    TaskId::new(id)
}

/// `feature` holds `mid`, which holds the group `deep` over the leaf `leaf`; `other` is a
/// top-level leaf.
fn nested() -> PickGraph {
    PickGraph::new(
        vec![task("feature"), task("other")],
        [
            PickTask::group("feature", &["mid"]),
            PickTask::group("mid", &["deep"]),
            PickTask::group("deep", &["leaf"]),
            PickTask::leaf("leaf"),
            PickTask::leaf("other"),
        ],
    )
}

fn done_record() -> DoneRecord {
    DoneRecord {
        tree_key: "tree".to_string(),
        generation: 1,
        at: SINCE.to_string(),
        evidence: Vec::new(),
    }
}

/// A store where `build-1` exists and owns each of `owners` in its state.
fn store(root: &TempDir, owners: &[(&str, OwnerState)]) -> Arc<StateWriter> {
    let writer = writer(root);
    writer
        .transact(|tx| {
            put_lane(tx, &LaneRecord::new(&build(1), LaneState::Idle))?;
            for (id, state) in owners {
                put_owner(tx, &task(id), &owner(&build(1), *state))?;
            }
            Ok(())
        })
        .unwrap();
    writer
}

fn mark(writer: &StateWriter, id: &str) {
    writer
        .transact(|tx| put_marker(tx, id, &OwedMarker::default()))
        .unwrap();
}

fn mark_done(writer: &StateWriter, id: &str) {
    writer
        .transact(|tx| put_done(tx, id, &done_record()))
        .unwrap();
}

/// Runs the clearing over what the store holds now, with every lane in the table.
fn clear(writer: &StateWriter, graph: &PickGraph) -> anyhow::Result<Vec<TaskId>> {
    let seen = done_owners_of(&writer.read(read_state).unwrap());
    writer.transact(|tx| clear_done_owners_in(tx, &seen, graph, &|_: &LaneId| true, SINCE))
}

fn done_owners_of(state: &LanesState) -> BTreeMap<TaskId, OwnerRecord> {
    state
        .owners
        .iter()
        .filter(|(_, owner)| owner.state == OwnerState::Done)
        .map(|(id, owner)| (id.clone(), owner.clone()))
        .collect()
}

fn owners(writer: &StateWriter) -> Vec<String> {
    writer
        .read(read_state)
        .unwrap()
        .owners
        .keys()
        .map(|id| id.0.clone())
        .collect()
}

fn history(writer: &StateWriter, id: &str) -> Vec<String> {
    writer
        .read(|read| read.task(id))
        .unwrap()
        .map(|record| {
            record
                .history
                .into_iter()
                .map(|entry| entry.event)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_done_owner_stays_while_any_task_under_it_owes_checks() {
    let root = TempDir::new().unwrap();
    let writer = store(&root, &[("feature", OwnerState::Done)]);
    let graph = nested();
    // A marker on a group three levels down, and on the leaf below it.
    mark(&writer, "deep");
    mark(&writer, "leaf");
    // A marker with its done record owes nothing; one under another top-level task does not
    // count.
    mark(&writer, "mid");
    mark_done(&writer, "mid");
    mark(&writer, "other");

    assert_eq!(clear(&writer, &graph).unwrap(), Vec::<TaskId>::new());
    mark_done(&writer, "deep");
    assert_eq!(clear(&writer, &graph).unwrap(), Vec::<TaskId>::new());
    assert_eq!(owners(&writer), ["feature"]);
    assert!(history(&writer, "feature").is_empty());

    mark_done(&writer, "leaf");
    assert_eq!(clear(&writer, &graph).unwrap(), [task("feature")]);
    assert!(owners(&writer).is_empty());
    assert_eq!(history(&writer, "feature"), [CLEARED_NO_CHECKS]);
}

#[test]
fn a_marker_on_the_top_level_task_itself_counts() {
    let root = TempDir::new().unwrap();
    let writer = store(&root, &[("feature", OwnerState::Done)]);
    mark(&writer, "feature");
    assert!(clear(&writer, &nested()).unwrap().is_empty());
    mark_done(&writer, "feature");
    assert_eq!(clear(&writer, &nested()).unwrap(), [task("feature")]);
}

#[test]
fn clearing_never_touches_a_holding_owner_or_a_record_that_changed() {
    let root = TempDir::new().unwrap();
    let writer = store(
        &root,
        &[
            ("a", OwnerState::Active),
            ("s", OwnerState::Stopped),
            ("h", OwnerState::Halted),
            ("feature", OwnerState::Done),
        ],
    );
    let graph = PickGraph::new(
        vec![task("a"), task("s"), task("h"), task("feature")],
        ["a", "s", "h", "feature"].map(PickTask::leaf),
    );
    // The done owner was seen before another writer changed it.
    let mut seen = done_owners_of(&writer.read(read_state).unwrap());
    seen.get_mut(&task("feature")).unwrap().since = "2026-02-01T10:00:00Z".to_string();
    let cleared = writer
        .transact(|tx| clear_done_owners_in(tx, &seen, &graph, &|_: &LaneId| true, SINCE))
        .unwrap();
    assert!(cleared.is_empty());
    assert_eq!(owners(&writer), ["a", "feature", "h", "s"]);

    // Holding owners are never cleared, even when passed as seen.
    let all = writer.read(read_state).unwrap().owners;
    let cleared = writer
        .transact(|tx| clear_done_owners_in(tx, &all, &graph, &|_: &LaneId| true, SINCE))
        .unwrap();
    assert_eq!(cleared, [task("feature")]);
    assert_eq!(owners(&writer), ["a", "h", "s"]);
    for id in ["a", "s", "h"] {
        assert!(history(&writer, id).is_empty(), "{id}");
    }
}

#[test]
fn a_failed_read_an_unreadable_done_record_or_an_unplaced_marker_clears_nothing() {
    let root = TempDir::new().unwrap();
    let writer = store(&root, &[("feature", OwnerState::Done)]);
    let graph = nested();
    let seen = done_owners_of(&writer.read(read_state).unwrap());

    // A done record that does not read leaves the task owing.
    mark(&writer, "leaf");
    writer
        .transact(|tx| {
            tx.kv(DONE_NAMESPACE)
                .put("leaf", toml::Value::String("junk".to_string()))
        })
        .unwrap();
    assert!(clear(&writer, &graph).unwrap().is_empty());
    mark_done(&writer, "leaf");

    // A task owing checks that the graph does not place may be under the owner's task.
    mark(&writer, "unplaced");
    assert!(clear(&writer, &graph).unwrap().is_empty());
    mark_done(&writer, "unplaced");

    // A read that fails aborts the transaction: nothing is cleared.
    writer
        .transact(|tx| {
            tx.kv(LANES_NAMESPACE)
                .put("owner/broken", toml::Value::Integer(5))
        })
        .unwrap();
    let result =
        writer.transact(|tx| clear_done_owners_in(tx, &seen, &graph, &|_: &LaneId| true, SINCE));
    assert!(result.is_err());
    let kept = writer
        .read(|read| read.kv(LANES_NAMESPACE).get("owner/feature"))
        .unwrap();
    assert!(kept.is_some());
    assert!(history(&writer, "feature").is_empty());
}

#[test]
fn a_done_owner_whose_lane_is_gone_is_cleared_even_while_checks_are_owed() {
    let root = TempDir::new().unwrap();
    let writer = store(&root, &[("feature", OwnerState::Done)]);
    mark(&writer, "leaf");
    let seen = done_owners_of(&writer.read(read_state).unwrap());

    // Out of the lane table's counts.
    let cleared = writer
        .transact(|tx| clear_done_owners_in(tx, &seen, &nested(), &|_: &LaneId| false, SINCE))
        .unwrap();
    assert_eq!(cleared, [task("feature")]);
    assert_eq!(
        history(&writer, "feature"),
        ["lane owner cleared: lane build-1 no longer exists"]
    );
}

#[test]
fn a_clear_is_one_transaction() {
    let root = TempDir::new().unwrap();
    let writer = store(&root, &[("feature", OwnerState::Done)]);
    let graph = nested();
    let seen = done_owners_of(&writer.read(read_state).unwrap());
    let generation = writer.generation().unwrap();

    // The history entry fails after the owner record was deleted: neither is written.
    let failed = writer
        .transact(|tx| clear_done_owners_in(tx, &seen, &graph, &|_: &LaneId| true, "no time"));
    assert!(failed.is_err());
    assert_eq!(owners(&writer), ["feature"]);
    assert!(history(&writer, "feature").is_empty());
    // A clear in a transaction that fails later is not written either.
    let failed = writer.transact(|tx| -> anyhow::Result<()> {
        clear_done_owners_in(tx, &seen, &graph, &|_: &LaneId| true, SINCE)?;
        anyhow::bail!("a later write fails")
    });
    assert!(failed.is_err());
    assert_eq!(owners(&writer), ["feature"]);
    assert!(history(&writer, "feature").is_empty());
    assert_eq!(writer.generation().unwrap(), generation);

    // A clear that commits deletes the record and writes its history in one generation.
    let mut events = writer.subscribe();
    assert_eq!(clear(&writer, &graph).unwrap(), [task("feature")]);
    assert_eq!(writer.generation().unwrap(), generation + 1);
    let Some(StateEvent::Changed {
        generation: changed,
        task_ids,
        namespaces,
    }) = futures::executor::block_on(events.next())
    else {
        panic!("no event");
    };
    assert_eq!(changed, generation + 1);
    let feature: Vec<bongos_work_model::TaskId> = vec!["feature".into()];
    assert_eq!(task_ids, feature);
    assert!(namespaces.iter().any(|name| name == LANES_NAMESPACE));
    assert!(owners(&writer).is_empty());
    assert_eq!(history(&writer, "feature"), [CLEARED_NO_CHECKS]);
}

/// The lanes' trees over a fresh lanes state, reading the checks owed from the harness's store,
/// registered in the app.
fn lane_trees(harness: &Harness, cx: &mut TestAppContext) -> LaneTaskTrees {
    let runs = LaneRuns::default();
    let trees = LaneTaskTrees::new(
        runs.clone(),
        CurrentJobTrees::default(),
        no_top_level(),
        process(&[]),
    );
    let writer = harness.writer.clone();
    trees.set_owed(Arc::new(move || writer.read(owing_tasks)));
    cx.update(|cx| {
        cx.set_global(runs);
        trees.register(cx);
    });
    trees
}

/// `feature` holds the finished leaf `leaf`; `other` is a ready top-level leaf.
fn feature_and_other() -> PickGraph {
    let mut tasks = vec![
        PickTask::group("feature", &["leaf"]),
        leaf("leaf"),
        leaf("other"),
    ];
    finish(&mut tasks, "leaf");
    PickGraph::new(vec![task("feature"), task("other")], tasks)
}

#[gpui::test]
async fn a_done_owner_lends_while_its_lane_works_elsewhere_until_the_done_record_clears_it(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let harness = harness(&builds(1), feature_and_other(), &[]);
    let trees = lane_trees(&harness, cx);
    // An earlier start made and checked the lane's second trees.
    let started = start_job(&fixture, &trees, RUN_1).await;
    let mut lane_setup = setup(&fixture, Vec::new());
    lane_setup.second_checked = started.second_checked();
    *harness.host.lane_trees.borrow_mut() = Some(lane_setup);
    let done = owner(&build(1), OwnerState::Done);
    harness
        .writer
        .transact(|tx| put_owner(tx, &task("feature"), &done))
        .unwrap();
    mark(&harness.writer, "leaf");

    let _started = start(&harness, start_request(), cx).await;

    // The lane picked other work; its done owner stays while checks are owed and lends its tree.
    assert_eq!(harness.engine.job_tasks(), ["other"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.state().owners[&task("feature")], done);
    assert_eq!(trees.lanes("project-1"), [build(1)], "set at the job start");
    let SecondTree::Lease(lease) = trees.second_tree("leaf", "app") else {
        panic!("not lent");
    };
    assert_eq!(lease.path(), path_of(&fixture, "tree:app:base"));
    assert_eq!(answer(&trees, "feature", "app"), "busy");
    drop(lease);
    assert_eq!(answer(&trees, "feature", "app"), "lease");

    // The done record: the next pass clears the owner, and nothing is lent.
    mark_done(&harness.writer, "leaf");
    cx.run_until_parked();

    assert!(!harness.state().owners.contains_key(&task("feature")));
    assert_eq!(history_of(&harness, "feature"), [CLEARED_NO_CHECKS]);
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
}

#[gpui::test]
async fn a_done_owner_with_nothing_owed_is_cleared_at_the_next_pass(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), feature_and_other(), &[]);
    let _trees = lane_trees(&harness, cx);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.engine.job_tasks(), ["other"]);

    // A done owner written while the lane works elsewhere, with no marker under its task.
    harness
        .writer
        .transact(|tx| put_owner(tx, &task("feature"), &owner(&build(1), OwnerState::Done)))
        .unwrap();
    cx.run_until_parked();

    assert!(!harness.state().owners.contains_key(&task("feature")));
    assert_eq!(history_of(&harness, "feature"), [CLEARED_NO_CHECKS]);
}

#[gpui::test]
async fn a_done_owner_of_a_lane_gone_from_the_table_is_cleared_and_the_lane_forgotten(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let harness = harness(&builds(1), feature_and_other(), &[]);
    let trees = lane_trees(&harness, cx);
    trees.set_lane("project-1", build(2), setup(&fixture, Vec::new()));
    harness
        .writer
        .transact(|tx| put_owner(tx, &task("feature"), &owner(&build(2), OwnerState::Done)))
        .unwrap();
    mark(&harness.writer, "leaf");

    let _started = start(&harness, start_request(), cx).await;

    assert!(!harness.state().owners.contains_key(&task("feature")));
    assert_eq!(
        history_of(&harness, "feature"),
        ["lane owner cleared: lane build-2 no longer exists"]
    );
    assert!(!trees.lanes("project-1").contains(&build(2)));
    assert_eq!(answer(&trees, "leaf", "app"), "unavailable");
}

#[gpui::test]
async fn a_pass_in_another_project_keeps_this_projects_top_level_lookup_and_lanes(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::build(TABLE, |_| None);
    let first = harness_in("project-1", &builds(1), feature_and_other());
    let trees = lane_trees(&first, cx);
    let started = start_job(&fixture, &trees, RUN_1).await;
    let mut lane_setup = setup(&fixture, Vec::new());
    lane_setup.second_checked = started.second_checked();
    *first.host.lane_trees.borrow_mut() = Some(lane_setup);
    first
        .writer
        .transact(|tx| put_owner(tx, &task("feature"), &owner(&build(1), OwnerState::Done)))
        .unwrap();
    mark(&first.writer, "leaf");
    let _first = start(&first, start_request(), cx).await;
    assert_eq!(trees.lanes("project-1"), [build(1)]);
    assert_eq!(answer(&trees, "leaf", "app"), "lease");

    // A second project with no `build-1` lane and a graph that does not know `leaf` passes.
    let docs = "[[kind]]\nname = \"docs\"\ncount = 1\n";
    let second = harness_in(
        "project-2",
        docs,
        PickGraph::new(vec![task("elsewhere")], [leaf("elsewhere")]),
    );
    let _second = start(&second, start_request(), cx).await;
    assert!(second.state().lanes.contains_key(&LaneId::new("docs", 1)));

    assert_eq!(trees.lanes("project-1"), [build(1)], "not forgotten");
    assert!(trees.lanes("project-2").is_empty());
    assert_eq!(answer(&trees, "leaf", "app"), "lease", "still looked up");
}

#[gpui::test]
async fn init_registers_the_lane_trees_where_the_checks_find_them(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        crate::init(cx);
    });
    let trees = cx
        .update(|cx| LaneTaskTrees::global(cx))
        .expect("registered");
    let registered = cx
        .update(|cx| TaskTreesRegistry::get(cx))
        .expect("the checks' provider");
    assert_eq!(
        answer(&trees, "leaf", "app"),
        "unavailable",
        "nothing known"
    );

    let fixture = Fixture::build(TABLE, |_| None);
    let started = start_job(&fixture, &trees, RUN_1).await;
    let mut lane_setup = setup(&fixture, Vec::new());
    lane_setup.second_checked = started.second_checked();
    trees.set_lane("project-1", build(1), lane_setup);
    trees.set_top_level(
        "project-1",
        Arc::new(|id| matches!(id, "feature" | "leaf").then(|| TaskId::new("feature"))),
    );
    let runs = cx.update(|cx| cx.global::<LaneRuns>().clone());
    runs.set(
        "project-1",
        LanesState {
            owners: [(task("feature"), owner(&build(1), OwnerState::Active))].into(),
            lanes: [(build(1), LaneRecord::new(&build(1), LaneState::Idle))].into(),
        },
    );

    let SecondTree::Lease(lease) = registered.second_tree("leaf", "app") else {
        panic!("not lent through the registry");
    };
    assert_eq!(lease.path(), path_of(&fixture, "tree:app:base"));
    assert!(trees.leases().is_held(lease.path()), "the same trees");
}
