//! The engine host through its real socket: a project in a temporary folder with a real state
//! store and git, its writer announced and served by this process, and clients that connect to
//! it as a second process would.

use std::cell::RefCell;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_checks::{ChecksAccess, ChecksCommand, ChecksReply, checks_access};
use bongos_project_repo::{
    ApprovalBasis, ClientKind, Git, Identity, Location, PersonClaim, ProjectRepo, RepoCommand,
    StateApplyHooks, StateHandle, StateOp, StateStore,
};
use bongos_project_repo_model::Author;
use futures::StreamExt as _;
use futures::executor::block_on;
use gpui::{App, Task, TestAppContext};
use serde_json::json;
use tempfile::TempDir;

use crate::{
    ClientMessage, ClientOptions, EngineAccess, HostClient, HostCommand, HostCommands, HostEvent,
    HostInfo, HostMode, LanesVerb, OpenedState, PROTOCOL_VERSION, ServerMessage, WorkflowVerb,
    WriterHost, WriterOptions, checks_handler, live_host, not_answering, open_state_with_hooks,
    read_host_file, register_checks, version_refusal, write_host_file,
};

const VERSION: &str = "1.2.3";

struct Fixture {
    root: TempDir,
    repo: ProjectRepo,
}

impl Fixture {
    fn location(&self) -> &Location {
        self.repo.location()
    }

    fn hosts_dir(&self) -> PathBuf {
        self.root.path().join("hosts")
    }

    fn log(&self) -> PathBuf {
        self.root.path().join("host.log")
    }

    fn writer_options(&self, serve: bool) -> WriterOptions {
        WriterOptions {
            mode: HostMode::Window,
            bongos_version: VERSION.to_string(),
            hosts_dir: self.hosts_dir(),
            serve_socket: serve,
        }
    }

    fn client_options(&self, client: ClientKind) -> ClientOptions {
        ClientOptions::new(client, VERSION, self.log())
    }

    fn connect(&self, client: ClientKind, cx: &TestAppContext) -> HostClient {
        let info = live_host(self.location()).expect("a live writer");
        HostClient::connect(
            &info.socket.expect("a socket"),
            self.client_options(client),
            cx.executor(),
        )
        .unwrap()
    }
}

fn definition(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
         created = 2026-01-01T10:00:00Z\n"
    )
}

/// A project in a real git repository inside a new temporary folder, its writer open in this
/// process with hooks of its own.
fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let folder = root.path().join("project");
    let location = Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state: folder.join("state"),
        folder,
        remote: None,
        converted_from: None,
    };
    std::fs::create_dir_all(&location.state).unwrap();
    std::fs::create_dir_all(&location.files).unwrap();
    let definitions = location.repository.join("definitions");
    for (path, text) in [
        (
            "project.toml",
            "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n".to_string(),
        ),
        ("task.toml", definition("g1", "Invoicing")),
        ("plan.md", String::new()),
        ("tasks/backfill/task.toml", definition("l1", "Backfill")),
    ] {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let git = Git::locate().expect("git is installed").isolated();
    block_on(async {
        git.init_repo(&location.repository, bongos_project_repo::MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer = StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap();
    let repo = ProjectRepo::new(location, git).with_state(StateHandle::Writer(Arc::new(writer)));
    Fixture { root, repo }
}

fn set_branch(task: &str, branch: &str) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: "branch".to_string(),
        expected: None,
        value: Some(toml::Value::String(branch.to_string())),
    }
}

fn branch_of(handle: &StateHandle, task: &str) -> Option<String> {
    handle
        .read(|read| read.task(task))
        .unwrap()
        .and_then(|record| record.branch)
}

fn start_writer(fixture: &Fixture, serve: bool, cx: &mut TestAppContext) -> WriterHost {
    cx.update(|cx| WriterHost::start(fixture.repo.clone(), fixture.writer_options(serve), cx))
        .unwrap()
}

#[gpui::test]
async fn a_second_process_gets_held_by_attaches_and_writes_only_through_the_server(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let host = start_writer(&fixture, true, cx);

    // host.json names this process, live, with the socket it serves.
    let info = read_host_file(fixture.location()).expect("host.json");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.mode, HostMode::Window);
    assert_eq!(info.bongos_version, VERSION);
    assert!(info.is_live());
    assert_eq!(
        info.socket.as_deref(),
        host.server().map(|server| server.socket())
    );

    // A second opener: the lock is held, so it attaches as a client with a reader.
    let opened = open_state_with_hooks(
        fixture.location(),
        &StateApplyHooks::new(),
        fixture.client_options(ClientKind::Window),
        cx.executor(),
    )
    .unwrap();
    let OpenedState::Client {
        client,
        reader,
        host: seen,
    } = opened
    else {
        panic!("the second opener took the writer");
    };
    assert_eq!(seen, info);
    assert_eq!(client.welcomed().writer_pid, std::process::id());
    assert_eq!(client.welcomed().mode, HostMode::Window);
    let mut events = client.subscribe(None).unwrap();

    // The client's repository has no writer; its writes go through the sink.
    let client_repo = ProjectRepo::new(fixture.location().clone(), fixture.repo.git().clone())
        .with_state(StateHandle::Reader(Arc::new(reader)));
    client_repo.replace_sink(Arc::new(client.clone()));
    client_repo
        .apply_state(vec![set_branch("l1", "lane/one")], Author::agent("chat 1"))
        .await
        .unwrap();

    // The write arrived through the server: the writer's store holds it, and the client reads
    // it from its own reader.
    let writer_state = fixture.repo.state().unwrap();
    assert_eq!(branch_of(writer_state, "l1").as_deref(), Some("lane/one"));
    let client_state = client_repo.state().unwrap();
    assert_eq!(branch_of(client_state, "l1").as_deref(), Some("lane/one"));
    // The client never opened the writer.
    assert!(client_state.writer().is_none());

    // ... and heard of it.
    let event = events.next().await.expect("an event");
    match event {
        HostEvent::StateChanged { task_ids, .. } => assert_eq!(task_ids, ["l1"]),
        other => panic!("expected StateChanged, got {other:?}"),
    }

    // Without a sink a reader-only repository refuses to write.
    let lone = ProjectRepo::new(fixture.location().clone(), fixture.repo.git().clone());
    assert!(
        lone.apply_state(vec![set_branch("l1", "x")], Author::agent("chat 1"))
            .await
            .is_err()
    );
    drop(client);
    drop(host);
    assert!(
        read_host_file(fixture.location()).is_none(),
        "host.json is removed"
    );
}

#[gpui::test]
async fn with_lanes_off_the_window_serves_no_socket_and_a_second_process_is_refused(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let mut host = start_writer(&fixture, false, cx);
    let info = read_host_file(fixture.location()).expect("host.json");
    assert!(info.is_live());
    assert_eq!(info.socket, None);
    assert!(!host.is_serving());
    assert!(!fixture.hosts_dir().exists());

    let refused = open_state_with_hooks(
        fixture.location(),
        &StateApplyHooks::new(),
        fixture.client_options(ClientKind::Window),
        cx.executor(),
    )
    .err()
    .expect("refused");
    assert!(refused.to_string().contains("lanes off"), "{refused:#}");

    // Turning lanes on serves the socket and names it.
    cx.update(|cx| host.set_serving(true, cx)).unwrap();
    let socket = read_host_file(fixture.location())
        .unwrap()
        .socket
        .expect("a socket");
    assert!(socket.exists());
    cx.update(|cx| host.set_serving(false, cx)).unwrap();
    assert!(!socket.exists());
    assert_eq!(read_host_file(fixture.location()).unwrap().socket, None);
}

#[gpui::test]
async fn a_cli_peers_approve_with_a_person_claim_writes_no_approval_and_a_window_peers_does(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let repo = fixture.repo.clone();
    let agent = Author::agent("chat 1");
    let at = chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let copy = block_on(repo.create_working_copy("amber-otter-0001", "work/", &agent, at)).unwrap();
    let task = copy.path.join("definitions/tasks/alpha/task.toml");
    std::fs::create_dir_all(task.parent().unwrap()).unwrap();
    std::fs::write(&task, definition("a1", "alpha")).unwrap();
    block_on(
        repo.git()
            .commit_if_dirty(&copy.path, "add alpha", &Identity::default()),
    )
    .unwrap();
    let _host = start_writer(&fixture, true, cx);
    let approve = || {
        HostCommand::Repo(RepoCommand::Approve {
            change_id: copy.change_id.clone(),
            person: PersonClaim {
                name: "Kim".to_string(),
            },
        })
    };

    let cli = fixture.connect(ClientKind::Cli, cx);
    let refused = cli.request(approve()).await.unwrap_err();
    assert!(
        refused.to_string().contains("verified window"),
        "{refused:#}"
    );
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());

    let window = fixture.connect(ClientKind::Window, cx);
    window.request(approve()).await.unwrap();
    let rows = repo.approvals(&copy.change_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].basis, ApprovalBasis::PersonApprove);
}

#[gpui::test]
async fn a_protocol_or_version_mismatch_is_refused_with_the_version_text(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let host = start_writer(&fixture, true, cx);
    let socket = host.server().unwrap().socket().to_path_buf();
    let expected = version_refusal(VERSION);
    assert_eq!(
        expected,
        "the engine host runs bongos 1.2.3; stop it with bongos host stop when its jobs end"
    );

    // A newer protocol, by hand.
    let mut stream = UnixStream::connect(&socket).unwrap();
    let hello = ClientMessage::Hello {
        protocol: PROTOCOL_VERSION + 1,
        bongos_version: VERSION.to_string(),
        client: ClientKind::Window,
    };
    stream
        .write_all(&crate::protocol::encode_line(&hello).unwrap())
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<ServerMessage>(&line).unwrap(),
        ServerMessage::Refused {
            text: expected.clone()
        }
    );

    // Another bongos version, through the client.
    let options = ClientOptions::new(ClientKind::Cli, "9.9.9", fixture.log());
    let error = HostClient::connect(&socket, options, cx.executor())
        .err()
        .expect("refused");
    assert_eq!(error.to_string(), expected);
}

#[gpui::test]
async fn a_client_windows_run_checks_runs_in_the_writer_and_a_cli_peer_passes_no_person(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let _host = start_writer(&fixture, true, cx);
    // The writer's `checks` handler, recording what reaches it.
    let seen = Rc::new(RefCell::new(Vec::<(serde_json::Value, bool)>::new()));
    let log = seen.clone();
    cx.update(|cx| {
        HostCommands::register(
            "checks",
            checks_handler(move |payload, person, _: &mut App| {
                log.borrow_mut().push((payload, person));
                Task::ready(ChecksReply::Started.to_value())
            }),
            cx,
        )
    });

    // A client window hooks task checks up to the writer: `task: run checks` is forwarded.
    let window = EngineAccess::Remote(fixture.connect(ClientKind::Window, cx));
    cx.update(|cx| window.hook_up_checks(cx));
    let access = cx.update(|cx| checks_access(cx)).expect("an access");
    assert!(matches!(access, ChecksAccess::Forwarded(_)));
    let command = ChecksCommand::RunChecks {
        task_id: "l1".to_string(),
        checks: None,
        clean: false,
    };
    let reply = cx.update(|cx| access.send(&command, cx)).await.unwrap();
    assert_eq!(reply, ChecksReply::Started);

    // The command-line client's request carries no person.
    let cli = fixture.connect(ClientKind::Cli, cx);
    cli.extension("checks", serde_json::to_value(&command).unwrap())
        .await
        .unwrap();
    let payload =
        json!({ "command": "run-checks", "task_id": "l1", "checks": null, "clean": false });
    assert_eq!(
        *seen.borrow(),
        [(payload.clone(), true), (payload.clone(), false)]
    );

    // The registered handler is task checks' own: with no checks engine in this writer it says
    // so, from the writer.
    cx.update(register_checks);
    let error = cli.extension("checks", payload).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "task checks are not running in this process"
    );

    // The writer itself runs the handler locally, and clears any forwarder.
    let local = EngineAccess::Local(fixture.repo.clone());
    cx.update(|cx| local.hook_up_checks(cx));
    assert!(matches!(
        cx.update(|cx| checks_access(cx)),
        None | Some(ChecksAccess::Local(_))
    ));
}

#[gpui::test]
async fn a_writer_that_does_not_answer_leaves_the_client_read_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let root = TempDir::new().unwrap();
    let socket = root.path().join("silent.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    // A writer that welcomes and then never answers, until the client hangs up.
    let silent = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let welcome = ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            bongos_version: VERSION.to_string(),
            writer_pid: 1,
            mode: HostMode::Host,
            generation: 0,
        };
        (&stream)
            .write_all(&crate::protocol::encode_line(&welcome).unwrap())
            .unwrap();
        while reader.read_line(&mut line).is_ok_and(|read| read > 0) {}
    });
    let log = root.path().join("host.log");
    let options = ClientOptions::new(ClientKind::Window, VERSION, log.clone());
    assert_eq!(options.answer_timeout, Duration::from_secs(10));
    let client = HostClient::connect(&socket, options, cx.executor()).unwrap();
    let expected = not_answering(&log);
    assert_eq!(
        expected,
        format!("the engine host is not answering; see {}", log.display())
    );

    let sink: Arc<dyn bongos_project_repo::StateSink> = Arc::new(client.clone());
    let write = sink.apply(vec![set_branch("l1", "x")], Author::agent("chat 1"));
    cx.run_until_parked();
    // Just short of the timeout the write still waits; at 10 s it gives up.
    cx.executor()
        .advance_clock(Duration::from_secs(10) - Duration::from_millis(1));
    assert!(!client.is_read_only());
    cx.executor().advance_clock(Duration::from_millis(1));
    let error = write.await.unwrap_err();
    assert_eq!(error.to_string(), expected);
    assert!(client.is_read_only());
    // Read-only now: the next write is refused at once with the same words.
    let error = sink
        .apply(vec![set_branch("l1", "y")], Author::agent("chat 1"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), expected);
    drop(sink);
    drop(client);
    silent.join().unwrap();
}

fn other_start(info: &HostInfo) -> Option<u64> {
    Some(info.start_time.map_or(1, |time| time + 1))
}

#[gpui::test]
async fn a_host_file_naming_a_dead_pid_or_another_start_time_is_never_live_and_a_stale_socket_is_removed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let live = HostInfo::this_process(
        HostMode::Host,
        None,
        VERSION,
        chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );
    assert!(live.is_live());
    let restarted = HostInfo {
        start_time: other_start(&live),
        ..live.clone()
    };
    assert!(!restarted.is_live());
    // No pid is this high on any system bongos runs on.
    let dead = HostInfo {
        pid: 0x7fff_fff0,
        ..live
    };
    assert!(!dead.is_live());
    for info in [&restarted, &dead] {
        write_host_file(fixture.location(), info).unwrap();
        assert_eq!(read_host_file(fixture.location()).as_ref(), Some(info));
        assert!(live_host(fixture.location()).is_none());
    }

    // A socket file left by a writer that died: nobody answers on it, so serving removes it.
    let socket = crate::socket_path(&fixture.hosts_dir(), &fixture.location().id);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    assert!(UnixStream::connect(&socket).is_err());
    let host = start_writer(&fixture, true, cx);
    assert_eq!(host.server().unwrap().socket(), socket.as_path());
    let client = fixture.connect(ClientKind::Cli, cx);
    assert_eq!(client.welcomed().bongos_version, VERSION);
    assert!(socket_mode(&socket) & 0o077 == 0);
    assert!(socket_mode(socket.parent().unwrap()) & 0o077 == 0);
}

fn socket_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode()
}

// A rerouted `workflow:` or `lanes:` command from a client reaches the handler the writer
// registered, with its verb, its payload and whether the peer may speak for a person.
#[gpui::test]
async fn rerouted_workflow_and_lanes_commands_from_a_client_reach_the_writers_handlers(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let _host = start_writer(&fixture, true, cx);
    let workflow = Rc::new(RefCell::new(Vec::<(String, serde_json::Value, bool)>::new()));
    let lanes = Rc::new(RefCell::new(Vec::<(String, serde_json::Value, bool)>::new()));
    let (seen_workflow, seen_lanes) = (workflow.clone(), lanes.clone());
    cx.update(|cx| {
        HostCommands::serve_workflow(
            Rc::new(move |verb, payload, peer, _: &mut App| {
                seen_workflow.borrow_mut().push((
                    serde_json::to_value(verb).unwrap().to_string(),
                    payload,
                    peer.may_attest_person(),
                ));
                Task::ready(Ok(json!("Workflow run resumed")))
            }),
            cx,
        );
        HostCommands::serve_lanes(
            Rc::new(move |verb, payload, peer, _: &mut App| {
                seen_lanes.borrow_mut().push((
                    serde_json::to_value(verb).unwrap().to_string(),
                    payload,
                    peer.may_attest_person(),
                ));
                Task::ready(Ok(json!({ "message": "done" })))
            }),
            cx,
        );
    });

    let window = EngineAccess::Remote(fixture.connect(ClientKind::Window, cx));
    let answer = cx
        .update(|cx| {
            window.request(
                HostCommand::Workflow {
                    verb: WorkflowVerb::Resume,
                    payload: json!({ "task_id": "l1", "run": null }),
                },
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(answer, json!("Workflow run resumed"));
    let adopt = json!({ "task": "l1", "lane": "build-2", "reason": "why", "person": "kim" });
    let cli = EngineAccess::Remote(fixture.connect(ClientKind::Cli, cx));
    for access in [&window, &cli] {
        cx.update(|cx| {
            access.request(
                HostCommand::Lanes {
                    verb: LanesVerb::Adopt,
                    payload: adopt.clone(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    }

    assert_eq!(
        *workflow.borrow(),
        [(
            "\"resume\"".to_string(),
            json!({ "task_id": "l1", "run": null }),
            true
        )]
    );
    // The same person claim reaches the writer from both peers; only the window's may be
    // trusted, which the handler reads from the peer, not from the payload.
    assert_eq!(
        *lanes.borrow(),
        [
            ("\"adopt\"".to_string(), adopt.clone(), true),
            ("\"adopt\"".to_string(), adopt, false)
        ]
    );
}

// In the writer itself a rerouted command runs the handler in this process, as a window.
#[gpui::test]
async fn a_rerouted_command_in_the_writer_runs_the_handler_locally(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let seen = Rc::new(RefCell::new(Vec::<bool>::new()));
    let log = seen.clone();
    cx.update(|cx| {
        HostCommands::serve_workflow(
            Rc::new(move |_, _, peer, _: &mut App| {
                log.borrow_mut().push(peer.may_attest_person());
                Task::ready(Ok(json!("ok")))
            }),
            cx,
        )
    });
    let local = EngineAccess::Local(fixture.repo.clone());
    let answer = cx
        .update(|cx| {
            local.request(
                HostCommand::Workflow {
                    verb: WorkflowVerb::Pause,
                    payload: json!({}),
                },
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(answer, json!("ok"));
    assert_eq!(*seen.borrow(), [true]);
}
