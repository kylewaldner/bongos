//! Workflow nodes that wait for their pools: feature 20's runtime with its scripted runner, over
//! real brokers and stores in temporary directories. The liveness of every process and the clock
//! are the test's; nothing sleeps.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use bongos_pools_model::{Level, PoolKind, PoolTable};
use bongos_workflow::process::ProcessRecord;
use bongos_workflow::runtime::fake::FakeRunner;
use bongos_workflow::runtime::{Finish, NodeStart, RunSpec, Runner, Runtime};
use bongos_workflow::{NEEDS_NOT_AVAILABLE, NodeNeeds};
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{
    Definition, Event, Failure, FailureReason, Limits, NodeKey, Severity, parse,
};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::broker_tests::{Project, World, ask, lease, need, pool, watch};
use crate::store_test_support::host;
use crate::{PoolConfig, PoolStore as _, PoolsForChecks, PoolsNodes, validate_node_needs};

/// The scripted runner, its processes alive as the world's probe sees them (a start time of
/// 1000 + pid).
#[derive(Default)]
struct LiveRunner(FakeRunner);

fn alive(record: ProcessRecord) -> ProcessRecord {
    ProcessRecord {
        pid: record.pid,
        start_time: 1_000 + u64::from(record.pid),
    }
}

impl Runner for LiveRunner {
    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        self.0.start(run, definition, key, try_number).map(alive)
    }

    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        self.0.start_node(node).map(alive)
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.0.poll()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        self.0.stop(tries)
    }

    fn merge_branch(&mut self, run: &str, join: &str, branch: &Branch) -> Result<String, Failure> {
        self.0.merge_branch(run, join, branch)
    }
}

const SINGLE: &str = r#"
schema = 1
name = "single"
start = "work"

[nodes.work]
kind = "agent"
prompt = "p"
needs = ["m"]
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// `a` waits at a gate while `b` runs holding `m`.
const GATED_PAIR: &str = r#"
schema = 1
name = "pair"
start = "split"

[nodes.split]
kind = "agent"
prompt = "p"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b"] }]

[nodes.a]
kind = "agent"
prompt = "p"
gate = { when = "before", question = "Go?" }
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "agent"
prompt = "p"
needs = ["m"]
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "agent"
prompt = "p"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// Both branches need the mutex `m`.
const SIBLINGS: &str = r#"
schema = 1
name = "siblings"
start = "split"

[nodes.split]
kind = "agent"
prompt = "p"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b"] }]

[nodes.a]
kind = "agent"
prompt = "p"
needs = ["m"]
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "agent"
prompt = "p"
needs = ["m"]
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "agent"
prompt = "p"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// `first` takes `h` (held while waiting), `second` waits at a gate and asks for `h` again,
/// `third` asks for `q`.
const KEEPING: &str = r#"
schema = 1
name = "keeping"
start = "first"

[nodes.first]
kind = "agent"
prompt = "p"
needs = ["h"]
output = { summary = { type = "text" } }
routes = [{ to = "second" }]

[nodes.second]
kind = "agent"
prompt = "p"
needs = ["h"]
gate = { when = "before", question = "Go on?" }
output = { summary = { type = "text" } }
routes = [{ to = "third" }]

[nodes.third]
kind = "agent"
prompt = "p"
needs = ["q"]
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn run_id(n: u32) -> String {
    format!("01890a5d-ac96-774b-bcce-b302{n:08x}")
}

fn key(node: &str) -> NodeKey {
    NodeKey::new(node, 1)
}

fn item(node: &str, index: u32) -> NodeKey {
    NodeKey::new(node, 1).in_branch("split", index)
}

fn keeping(name: &str) -> PoolConfig {
    PoolConfig {
        hold_while_waiting: true,
        ..pool(name, PoolKind::Mutex, 1)
    }
}

/// One project's broker, the nodes' side over it and a runtime asking it.
struct Bench {
    world: World,
    project: Project,
    nodes: Rc<PoolsNodes>,
    runtime: Runtime<LiveRunner>,
    temp: TempDir,
}

impl Bench {
    fn new(cx: &mut TestAppContext, pools: Vec<PoolConfig>) -> Bench {
        let world = World::new();
        let project = world.project(cx, "p", host(1), pools, vec![]);
        let brokers = PoolsForChecks::new();
        brokers.insert(project.broker.clone());
        let (nodes, admission) = PoolsNodes::new(
            brokers,
            Box::new(|_| Some("p".to_string())),
            cx.foreground_executor().clone(),
        );
        let mut runtime = Runtime::new(Limits::default(), LiveRunner::default());
        runtime.set_admission(Box::new(admission));
        Bench {
            world,
            project,
            nodes,
            runtime,
            temp: TempDir::new().unwrap(),
        }
    }

    fn add(&mut self, n: u32, text: &str) {
        self.runtime
            .add(RunSpec {
                id: run_id(n),
                definition: parse(text).unwrap(),
                attempt_dir: self.temp.path().join(format!("runs/{n}/attempts/1")),
                loop_bound: 5,
            })
            .unwrap();
    }

    /// Ticks the runtime and runs the brokers until both have nothing left to do.
    fn settle(&mut self, cx: &mut TestAppContext) {
        for _ in 0..4 {
            self.runtime.tick();
            cx.run_until_parked();
        }
    }

    fn runner(&mut self) -> &mut FakeRunner {
        &mut self.runtime.runner_mut().0
    }

    fn table(&self) -> PoolTable {
        self.project.project_store.read().unwrap()
    }

    /// The owners holding `pool`, with their levels.
    fn holders(&self, pool: &str) -> Vec<(String, Level)> {
        self.table().pools[pool]
            .holds
            .iter()
            .map(|hold| {
                (
                    hold.holder.path.owner.0.clone(),
                    hold.holder.path.level.clone(),
                )
            })
            .collect()
    }

    /// The owners waiting for `pool`, in line.
    fn waiters(&self, pool: &str) -> Vec<String> {
        self.table().pools[pool]
            .waiters
            .iter()
            .map(|waiter| waiter.holder.path.owner.0.clone())
            .collect()
    }

    fn started(&self, n: u32, node: &str) -> usize {
        self.runtime
            .events(&run_id(n))
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::NodeStarted { key, .. } if key.node == node))
            .count()
    }

    fn failure(&self, n: u32) -> Option<Failure> {
        self.runtime
            .events(&run_id(n))
            .unwrap()
            .iter()
            .find_map(|event| match event {
                Event::NodeFinished {
                    failure: Some(failure),
                    ..
                } => Some(failure.clone()),
                _ => None,
            })
    }
}

fn step(node: &str) -> Level {
    Level::Step {
        key: key(node).to_string(),
    }
}

#[gpui::test]
fn a_node_waiting_on_a_held_mutex_starts_only_when_the_holder_ends(cx: &mut TestAppContext) {
    let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
    let holder = watch(cx, bench.project.broker.acquire(ask("x", &[need("m")])));
    cx.run_until_parked();
    let holder = lease(&holder);
    bench.add(1, SINGLE);
    bench.settle(cx);
    assert_eq!(bench.started(1, "work"), 0, "no NodeStarted while it waits");
    assert!(
        bench.runner().started.is_empty(),
        "its time cap has not started"
    );
    assert_eq!(bench.runtime.sessions_in_flight(), 0);
    assert_eq!(bench.waiters("m"), [run_id(1)]);
    let status = bench.nodes.waiting_node(&run_id(1), &key("work")).unwrap();
    assert_eq!((status.pool.as_str(), status.position), ("m", 1));
    let waiting = bench.runtime.pool_waiting(&run_id(1));
    assert_eq!(waiting[0].1.text, "waiting for pool m");

    // Ten minutes pass; the node's try, and its timer, begin only at the grant.
    bench.world.advance(cx, Duration::from_secs(600));
    let status = bench.nodes.waiting_node(&run_id(1), &key("work")).unwrap();
    assert_eq!(
        crate::wait_line(&status, bench.project.broker.now().0),
        "waiting: m, place 1, held by holder x, 10m"
    );
    bench.settle(cx);
    assert!(bench.runner().started.is_empty());

    drop(holder);
    bench.settle(cx);
    assert_eq!(bench.started(1, "work"), 1);
    assert_eq!(bench.runner().started.len(), 1);
    assert_eq!(bench.holders("m"), [(run_id(1), step("work"))]);
    let bound = bench.table().pools["m"].holds[0].holder.process;
    assert!(bound.is_some(), "the hold lives with the node's process");
    assert!(bench.nodes.waiting_node(&run_id(1), &key("work")).is_none());

    bench.runner().release_all();
    bench.settle(cx);
    assert!(bench.holders("m").is_empty(), "released at the finish");
    assert!(bench.runtime.state(&run_id(1)).unwrap().is_over());
}

#[gpui::test]
fn halting_or_interrupting_releases_the_node_pool_and_resuming_asks_again(cx: &mut TestAppContext) {
    for interrupt in [false, true] {
        let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
        bench.add(1, SINGLE);
        bench.settle(cx);
        assert_eq!(bench.holders("m"), [(run_id(1), step("work"))]);
        if interrupt {
            assert!(bench.runtime.interrupt_all());
        } else {
            bench.runtime.halt(&run_id(1)).unwrap();
        }
        cx.run_until_parked();
        assert!(bench.holders("m").is_empty(), "interrupt: {interrupt}");

        // Someone else takes it meanwhile: the resume waits for it.
        let other = watch(cx, bench.project.broker.acquire(ask("x", &[need("m")])));
        cx.run_until_parked();
        let other = lease(&other);
        bench.runtime.resume(&run_id(1)).unwrap();
        bench.settle(cx);
        assert_eq!(bench.started(1, "work"), 1, "interrupt: {interrupt}");
        assert_eq!(bench.waiters("m"), [run_id(1)]);
        drop(other);
        bench.settle(cx);
        assert_eq!(bench.started(1, "work"), 2, "interrupt: {interrupt}");
        assert_eq!(bench.holders("m"), [(run_id(1), step("work"))]);
    }
}

#[gpui::test]
fn stopping_a_run_drops_its_waiting_keys_so_the_next_waiter_is_granted(cx: &mut TestAppContext) {
    for stop in ["halt", "cancel", "interrupt"] {
        let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
        let holder = watch(cx, bench.project.broker.acquire(ask("x", &[need("m")])));
        cx.run_until_parked();
        let holder = lease(&holder);
        bench.add(1, SINGLE);
        bench.settle(cx);
        let next = watch(cx, bench.project.broker.acquire(ask("z", &[need("m")])));
        cx.run_until_parked();
        assert_eq!(bench.waiters("m"), [run_id(1), "z".to_string()], "{stop}");
        match stop {
            "halt" => bench.runtime.halt(&run_id(1)).unwrap(),
            "cancel" => bench.runtime.cancel(&run_id(1)).unwrap(),
            _ => {
                bench.runtime.interrupt_all();
            }
        }
        cx.run_until_parked();
        assert_eq!(bench.waiters("m"), ["z"], "{stop}");
        drop(holder);
        cx.run_until_parked();
        let _next = lease(&next);
        assert_eq!(
            bench.holders("m"),
            [("z".to_string(), Level::Job)],
            "{stop}"
        );
        assert_eq!(bench.started(1, "work"), 0, "{stop}");
    }
}

#[gpui::test]
fn a_gate_in_one_branch_leaves_another_branchs_running_node_holding_its_pool(
    cx: &mut TestAppContext,
) {
    let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
    bench.add(1, GATED_PAIR);
    bench.settle(cx);
    bench.runner().release_all();
    bench.settle(cx);
    let state = bench.runtime.state(&run_id(1)).unwrap();
    assert!(state.open_gates.contains_key(&item("a", 0)), "{state:?}");
    assert_eq!(bench.runtime.in_flight(&run_id(1)), [item("b", 1)]);
    let b = Level::Step {
        key: item("b", 1).to_string(),
    };
    assert_eq!(bench.holders("m"), [(run_id(1), b.clone())]);
    bench.world.advance(cx, Duration::from_secs(5));
    bench.settle(cx);
    assert_eq!(
        bench.holders("m"),
        [(run_id(1), b)],
        "the gate released nothing"
    );
}

#[gpui::test]
fn fan_out_siblings_queue_like_strangers(cx: &mut TestAppContext) {
    let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
    bench.add(1, SIBLINGS);
    bench.settle(cx);
    bench.runner().release_all();
    bench.settle(cx);
    assert_eq!(bench.runtime.in_flight(&run_id(1)).len(), 1);
    assert_eq!(bench.waiters("m"), [run_id(1)]);
    assert_eq!(bench.runtime.pool_waiting(&run_id(1)).len(), 1);
    bench.runner().release_all();
    bench.settle(cx);
    assert_eq!(bench.started(1, "a") + bench.started(1, "b"), 2);
    bench.runner().release_all();
    bench.settle(cx);
    bench.runner().release_all();
    bench.settle(cx);
    assert!(bench.runtime.state(&run_id(1)).unwrap().is_over());
    assert!(bench.holders("m").is_empty());
}

#[gpui::test]
fn a_hold_while_waiting_pool_is_kept_across_a_gate_and_released_at_the_end(
    cx: &mut TestAppContext,
) {
    let mut bench = Bench::new(cx, vec![keeping("h"), pool("q", PoolKind::Mutex, 1)]);
    bench.add(1, KEEPING);
    bench.settle(cx);
    assert_eq!(bench.holders("h"), [(run_id(1), step("first"))]);
    assert!(
        bench.table().pools["h"].holds[0].holder.process.is_none(),
        "kept apart from the node's process"
    );
    bench.runner().release_all();
    bench.settle(cx);
    // `first` finished: its hold of `h` is the run's now, and the run waits at the gate.
    assert_eq!(bench.holders("h"), [(run_id(1), Level::Run)]);
    assert_eq!(bench.nodes.run_pools(&run_id(1)), ["h"]);
    assert!(
        bench
            .runtime
            .state(&run_id(1))
            .unwrap()
            .open_gates
            .contains_key(&key("second"))
    );
    let stranger = watch(cx, bench.project.broker.acquire(ask("z", &[need("h")])));
    cx.run_until_parked();
    assert_eq!(bench.waiters("h"), ["z"]);

    bench
        .runtime
        .close_gate(&run_id(1), key("second"), "kyle", None, None)
        .unwrap();
    bench.settle(cx);
    // `second` is satisfied by the run's hold, ahead of the stranger.
    assert_eq!(bench.started(1, "second"), 1);
    assert_eq!(bench.holders("h"), [(run_id(1), Level::Run)]);
    bench.runner().release_all();
    bench.settle(cx);
    assert_eq!(bench.started(1, "third"), 1);
    assert_eq!(bench.holders("h"), [(run_id(1), Level::Run)]);
    assert!(stranger.borrow().is_none());

    bench.runner().release_all();
    bench.settle(cx);
    assert!(bench.runtime.state(&run_id(1)).unwrap().is_over());
    assert!(bench.nodes.run_pools(&run_id(1)).is_empty());
    let _granted = lease(&stranger);
    assert_eq!(bench.holders("h"), [("z".to_string(), Level::Job)]);
    assert!(bench.holders("q").is_empty());
}

#[gpui::test]
fn a_deadlocking_request_fails_the_node_with_pool_refused(cx: &mut TestAppContext) {
    let mut bench = Bench::new(cx, vec![keeping("h"), pool("q", PoolKind::Mutex, 1)]);
    let without_gate = KEEPING.replace("gate = { when = \"before\", question = \"Go on?\" }\n", "");
    bench.add(1, &without_gate);
    bench.settle(cx);
    // Job y holds q, and a step of y waits for h, which `first` holds.
    let y = watch(cx, bench.project.broker.acquire(ask("y", &[need("q")])));
    cx.run_until_parked();
    let _y = lease(&y);
    let y_step = bench.project.broker.acquire(crate::Request {
        needs: vec![need("h")],
        holder: crate::broker_tests::holder(
            "y",
            Level::Step {
                key: "copy".to_string(),
            },
        ),
    });
    cx.run_until_parked();
    // `first` ends: the run keeps h; `second` is covered by it; `third` asks for q, which y
    // holds while y waits for the run's h.
    for _ in 0..2 {
        bench.runner().release_all();
        bench.settle(cx);
    }
    let failure = bench.failure(1).expect("third failed");
    assert_eq!(failure.reason, FailureReason::PoolRefused);
    assert_eq!(failure.node, Some(key("third")));
    assert!(failure.detail.starts_with("would deadlock"), "{failure:?}");
    assert_eq!(
        bench.started(1, "third"),
        1,
        "recorded like a start failure"
    );
    assert!(bench.runtime.state(&run_id(1)).unwrap().ended.is_some());
    // The run ended: its kept hold went, and y's step is granted.
    cx.run_until_parked();
    assert_eq!(
        bench.holders("h"),
        [(
            "y".to_string(),
            Level::Step {
                key: "copy".to_string()
            }
        )]
    );
    drop(y_step);
}

#[gpui::test]
fn a_start_that_fails_after_the_grant_releases_the_pool(cx: &mut TestAppContext) {
    let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
    // An unbound run cannot be asked about running unsandboxed: the runner refuses the start.
    bench.runner().no_sandbox.insert("work".to_string());
    bench.add(1, SINGLE);
    bench.settle(cx);
    let failure = bench.failure(1).unwrap();
    assert_eq!(failure.reason, FailureReason::SandboxRefused);
    assert!(bench.holders("m").is_empty());
    assert!(bench.waiters("m").is_empty());
}

#[gpui::test]
fn an_undeclared_pool_fails_the_node(cx: &mut TestAppContext) {
    let mut bench = Bench::new(cx, vec![pool("m", PoolKind::Mutex, 1)]);
    bench.add(1, &SINGLE.replace("needs = [\"m\"]", "needs = [\"gone\"]"));
    bench.settle(cx);
    let failure = bench.failure(1).unwrap();
    assert_eq!(failure.reason, FailureReason::PoolRefused);
    assert_eq!(
        failure.detail,
        "pool `gone` is not declared in pools/pools.toml"
    );
    assert!(bench.runner().started.is_empty());
}

#[gpui::test]
fn several_runs_ask_their_own_projects_brokers(cx: &mut TestAppContext) {
    let world = World::new();
    let p = world.project(
        cx,
        "p",
        host(1),
        vec![pool("m", PoolKind::Mutex, 1)],
        vec![],
    );
    let r = world.project(
        cx,
        "r",
        host(1),
        vec![pool("m", PoolKind::Mutex, 1)],
        vec![],
    );
    let brokers = PoolsForChecks::new();
    brokers.insert(p.broker.clone());
    brokers.insert(r.broker.clone());
    let (_nodes, admission) = PoolsNodes::new(
        brokers,
        Box::new(|run| Some(if run == run_id(2) { "r" } else { "p" }.to_string())),
        cx.foreground_executor().clone(),
    );
    let mut runtime = Runtime::new(Limits::default(), LiveRunner::default());
    runtime.set_admission(Box::new(admission));
    let temp = TempDir::new().unwrap();
    for n in 1..=3 {
        runtime
            .add(RunSpec {
                id: run_id(n),
                definition: parse(SINGLE).unwrap(),
                attempt_dir: temp.path().join(format!("runs/{n}/attempts/1")),
                loop_bound: 5,
            })
            .unwrap();
    }
    for _ in 0..4 {
        runtime.tick();
        cx.run_until_parked();
    }
    let in_flight: BTreeSet<String> = (1..=3)
        .filter(|n| !runtime.in_flight(&run_id(*n)).is_empty())
        .map(run_id)
        .collect();
    // Runs 1 and 3 share p's mutex; run 2 has r's to itself.
    assert!(in_flight.contains(&run_id(2)));
    assert_eq!(in_flight.len(), 2, "{in_flight:?}");
    let owners = |project: &Project| -> Vec<String> {
        project.project_store.read().unwrap().pools["m"]
            .holds
            .iter()
            .map(|hold| hold.holder.path.owner.0.clone())
            .collect()
    };
    assert_eq!(owners(&r), [run_id(2)]);
    assert_eq!(owners(&p).len(), 1);
    runtime.runner_mut().0.release_all();
    for _ in 0..4 {
        runtime.tick();
        cx.run_until_parked();
    }
    assert_eq!(
        (1..=3)
            .filter(|n| runtime.state(&run_id(*n)).unwrap().is_over())
            .count(),
        2
    );
    assert_eq!(
        runtime.in_flight(&run_id(1)).len() + runtime.in_flight(&run_id(3)).len(),
        1
    );
}

#[test]
fn the_needs_validator_refuses_needs_while_pools_are_off() {
    let text = SINGLE.replace("needs = [\"m\"]", "needs = [{ pool = \"m\", size = 2 }]");
    let definition = parse(&text).unwrap();
    let node = &definition.nodes[0];
    crate::ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    let off = validate_node_needs(node);
    assert_eq!(off.len(), 1);
    assert_eq!(off[0].message, NEEDS_NOT_AVAILABLE);
    assert_eq!(
        off,
        NodeNeeds::not_available(node)
            .into_iter()
            .collect::<Vec<_>>()
    );

    crate::ENABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let on = validate_node_needs(node);
    assert_eq!(on.len(), 1);
    assert_eq!(on[0].severity, Severity::Error);
    assert_eq!(on[0].message, "unknown key \"size\" in a needs entry");
    let fine = parse(SINGLE).unwrap();
    assert!(validate_node_needs(&fine.nodes[0]).is_empty());
    crate::ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
}
