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
    Attach, ClientMessage, ClientOptions, EngineAccess, EngineHostProcess, EnsureHost, HostClient,
    HostCommand, HostCommands, HostEvent, HostInfo, HostMode, HostProcessOptions, HostSpawner,
    LanesVerb, OpenedState, PROTOCOL_VERSION, ServerMessage, SpawnedHost, WorkflowVerb, WriterHost,
    WriterHosts, WriterOptions, checks_handler, ensure_host, live_host, not_answering,
    open_host_writer, open_state_with_hooks, read_host_file, register_checks, version_refusal,
    write_host_file,
};

pub(crate) const VERSION: &str = "1.2.3";

pub(crate) struct Fixture {
    pub(crate) root: TempDir,
    pub(crate) repo: ProjectRepo,
}

impl Fixture {
    pub(crate) fn location(&self) -> &Location {
        self.repo.location()
    }

    pub(crate) fn hosts_dir(&self) -> PathBuf {
        self.root.path().join("hosts")
    }

    pub(crate) fn log(&self) -> PathBuf {
        self.root.path().join("host.log")
    }

    pub(crate) fn writer_options(&self, serve: bool) -> WriterOptions {
        WriterOptions {
            mode: HostMode::Window,
            bongos_version: VERSION.to_string(),
            hosts_dir: self.hosts_dir(),
            serve_socket: serve,
        }
    }

    pub(crate) fn client_options(&self, client: ClientKind) -> ClientOptions {
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
pub(crate) fn fixture() -> Fixture {
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

pub(crate) fn set_branch(task: &str, branch: &str) -> StateOp {
    StateOp::Set {
        task: task.to_string(),
        field: "branch".to_string(),
        expected: None,
        value: Some(toml::Value::String(branch.to_string())),
    }
}

pub(crate) fn branch_of(handle: &StateHandle, task: &str) -> Option<String> {
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

// `project: approve` and abandoning in a client window go to the writer, which writes the
// approval row and abandons the change; the client reads both back from its own reader.
#[gpui::test]
async fn a_client_windows_approve_and_abandon_reach_the_writer(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let repo = fixture.repo.clone();
    let agent = Author::agent("chat 1");
    let at = chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut copies = Vec::new();
    for (name, slug, id) in [
        ("amber-otter-0001", "alpha", "a1"),
        ("amber-otter-0002", "beta", "b1"),
    ] {
        let copy = block_on(repo.create_working_copy(name, "work/", &agent, at)).unwrap();
        let task = copy
            .path
            .join(format!("definitions/tasks/{slug}/task.toml"));
        std::fs::create_dir_all(task.parent().unwrap()).unwrap();
        std::fs::write(&task, definition(id, slug)).unwrap();
        block_on(
            repo.git()
                .commit_if_dirty(&copy.path, "add", &Identity::default()),
        )
        .unwrap();
        copies.push(copy);
    }
    let _host = start_writer(&fixture, true, cx);
    let OpenedState::Client { client, reader, .. } = open_state_with_hooks(
        fixture.location(),
        &StateApplyHooks::new(),
        fixture.client_options(ClientKind::Window),
        cx.executor(),
    )
    .unwrap() else {
        panic!("the second opener took the writer");
    };
    let client_repo = ProjectRepo::new(fixture.location().clone(), fixture.repo.git().clone())
        .with_state(StateHandle::Reader(Arc::new(reader)));
    client_repo.replace_sink(Arc::new(client));

    let keyboard = bongos_work_model::commands::Context::at_keyboard(
        bongos_work_model::PersonAtKeyboard::for_test(),
    );
    let approved = client_repo
        .approve(&copies[0].change_id, keyboard, at)
        .await
        .unwrap();
    assert_eq!(approved.basis, ApprovalBasis::PersonApprove);
    let rows = repo.approvals(&copies[0].change_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].basis, ApprovalBasis::PersonApprove);

    client_repo
        .abandon_now(&copies[1].change_id, at)
        .await
        .unwrap();
    let row = repo.change(&copies[1].change_id).unwrap().unwrap();
    assert_eq!(row.state, bongos_project_repo::ChangeState::Abandoned);
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
async fn a_stale_protocol_with_the_same_version_string_is_refused_and_the_current_one_is_welcomed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let host = start_writer(&fixture, true, cx);
    let socket = host.server().unwrap().socket().to_path_buf();

    let hello_with = |protocol: u32| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        let hello = ClientMessage::Hello {
            protocol,
            bongos_version: VERSION.to_string(),
            client: ClientKind::Window,
        };
        stream
            .write_all(&crate::protocol::encode_line(&hello).unwrap())
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<ServerMessage>(&line).unwrap()
    };

    // The same version string, one protocol behind the shared constant: a dev build's stale peer.
    assert_eq!(
        hello_with(bongos_version::ENGINE_HOST_PROTOCOL - 1),
        ServerMessage::Refused {
            text: version_refusal(VERSION)
        }
    );
    // The same version string and the shared protocol: welcomed.
    assert!(matches!(
        hello_with(bongos_version::ENGINE_HOST_PROTOCOL),
        ServerMessage::Welcome { .. }
    ));
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

// A client that stops reading never blocks a broadcast: its queue fills, its connection is
// ended, and the other clients still hear every later event.
#[gpui::test]
async fn a_client_that_never_reads_does_not_block_broadcast(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let host = start_writer(&fixture, true, cx);
    let socket = host.server().unwrap().socket().to_path_buf();

    // By hand on another thread: hello, subscribe to everything, then a request whose reply
    // proves the subscription is in place. After that the client reads nothing.
    let (ready, stuck) = futures::channel::oneshot::channel::<UnixStream>();
    std::thread::spawn(move || {
        let mut stream = UnixStream::connect(&socket).unwrap();
        for message in [
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                bongos_version: VERSION.to_string(),
                client: ClientKind::Window,
            },
            ClientMessage::Subscribe { topics: Vec::new() },
            ClientMessage::Request {
                id: 7,
                command: HostCommand::Host(crate::protocol::HostCtl::Status),
            },
        ] {
            stream
                .write_all(&crate::protocol::encode_line(&message).unwrap())
                .unwrap();
        }
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let ServerMessage::Reply { id: 7, .. } = serde_json::from_str(&line).unwrap() {
                break;
            }
        }
        ready.send(stream).ok();
    });
    let mut stuck = stuck.await.unwrap();

    // Far more than the socket's buffer and the queue hold; every call returns.
    let text = "x".repeat(4096);
    for _ in 0..(crate::server::OUTBOX_LINES * 8) {
        host.server()
            .unwrap()
            .broadcast(&HostEvent::Notice { text: text.clone() });
    }
    // The stuck connection was ended: reading it now comes to its end.
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut stuck, &mut rest).ok();

    let listener = fixture.connect(ClientKind::Window, cx);
    let mut events = listener
        .subscribe(Some(vec![crate::protocol::HostTopic::Notices]))
        .unwrap();
    // The subscription travels on its own; a request after it proves it arrived.
    listener
        .request(HostCommand::Host(crate::protocol::HostCtl::Status))
        .await
        .unwrap();
    host.server().unwrap().broadcast(&HostEvent::Notice {
        text: "after".to_string(),
    });
    assert_eq!(
        events.next().await,
        Some(HostEvent::Notice {
            text: "after".to_string()
        })
    );
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

// ---- The engine host process and `ensure_host` ----

const IDLE_EXIT: Duration = Duration::from_secs(300);

/// Starts this process as the fixture's engine host; the flag turns true when it exits.
fn start_host_process(fixture: &Fixture, cx: &mut TestAppContext) -> Rc<std::cell::Cell<bool>> {
    start_host_process_with(
        fixture,
        Rc::new(|_: &App| false),
        Rc::new(|_: &mut App| {}),
        cx,
    )
}

/// [`start_host_process`] with `busy` and `drain` given.
fn start_host_process_with(
    fixture: &Fixture,
    busy: crate::HostBusy,
    drain: crate::HostDrain,
    cx: &mut TestAppContext,
) -> Rc<std::cell::Cell<bool>> {
    let exited = Rc::new(std::cell::Cell::new(false));
    let on_exit = exited.clone();
    cx.update(|cx| {
        EngineHostProcess::start(
            fixture.repo.clone(),
            HostProcessOptions {
                bongos_version: VERSION.to_string(),
                hosts_dir: fixture.hosts_dir(),
                idle_exit: IDLE_EXIT,
            },
            busy,
            drain,
            Box::new(move |_| on_exit.set(true)),
            cx,
        )
    })
    .unwrap();
    exited
}

fn host_connections(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| {
        cx.try_global::<WriterHosts>()
            .and_then(|hosts| {
                hosts
                    .hosts()
                    .first()?
                    .server()
                    .map(|s| s.connection_count())
            })
            .unwrap_or(0)
    })
}

/// Waits for `condition`, letting the app run, with a generous guard against a hang.
pub(crate) fn wait_for(
    cx: &mut TestAppContext,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) {
    let guard = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if condition(cx) {
            return;
        }
        assert!(
            std::time::Instant::now() < guard,
            "the condition never held"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

// An engine host with nothing attached and nothing running exits once the idle time is up, on
// the executor's clock, and removes its host.json and socket.
#[gpui::test]
async fn an_idle_engine_host_exits_and_removes_its_files(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let exited = start_host_process(&fixture, cx);

    let info = live_host(fixture.location()).expect("a live writer");
    assert_eq!(info.mode, HostMode::Host);
    assert_eq!(info.pid, std::process::id());
    let socket = info.socket.expect("a socket");
    assert!(socket.exists());
    assert!(cx.update(|cx| EngineHostProcess::is_running(cx)));

    cx.run_until_parked();
    cx.executor()
        .advance_clock(IDLE_EXIT - Duration::from_secs(1));
    cx.run_until_parked();
    assert!(!exited.get(), "exited before the idle time was up");

    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(exited.get(), "did not exit once idle");
    assert!(read_host_file(fixture.location()).is_none());
    assert!(!socket.exists());
}

// `host stop` is answered, then the engine host stops serving, exits and removes its files, with a
// client still attached and long before the idle time.
#[gpui::test]
async fn a_stop_command_stops_the_engine_host_after_answering(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let exited = start_host_process(&fixture, cx);
    let socket = live_host(fixture.location())
        .and_then(|info| info.socket)
        .expect("a socket");
    let client = fixture.connect(ClientKind::Cli, cx);
    wait_for(cx, |cx| host_connections(cx) == 1);

    let answer = client
        .request(HostCommand::Host(crate::protocol::HostCtl::Stop {
            at_job_boundary: false,
        }))
        .await
        .unwrap();
    assert_eq!(answer["stopping"], json!(true));

    wait_for(cx, |_| exited.get());
    assert!(read_host_file(fixture.location()).is_none());
    assert!(!socket.exists());
}

// `host stop` at the job boundary first asks the dispatcher to drain, then stops only once nothing
// keeps the host busy, on the executor's clock.
#[gpui::test]
async fn a_stop_at_the_job_boundary_drains_the_dispatcher_then_waits(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let busy = Rc::new(std::cell::Cell::new(true));
    let drained = Rc::new(std::cell::Cell::new(0u32));
    let exited = {
        let busy = busy.clone();
        let drained = drained.clone();
        start_host_process_with(
            &fixture,
            Rc::new(move |_: &App| busy.get()),
            Rc::new(move |_: &mut App| drained.set(drained.get() + 1)),
            cx,
        )
    };
    let client = fixture.connect(ClientKind::Cli, cx);
    wait_for(cx, |cx| host_connections(cx) == 1);

    let answer = client
        .request(HostCommand::Host(crate::protocol::HostCtl::Stop {
            at_job_boundary: true,
        }))
        .await
        .unwrap();
    assert_eq!(answer["stopping"], json!(true));
    wait_for(cx, |_| drained.get() > 0);
    assert_eq!(drained.get(), 1, "the dispatcher is asked to drain once");

    // The running job has not finished: the host keeps serving.
    cx.executor().advance_clock(crate::BUSY_RECHECK * 3);
    cx.run_until_parked();
    assert!(!exited.get(), "stopped while the dispatcher was still busy");

    // The dispatcher's last job finished and it ended.
    busy.set(false);
    cx.executor().advance_clock(crate::BUSY_RECHECK);
    cx.run_until_parked();
    assert!(exited.get(), "did not stop once the dispatcher was idle");
    assert!(read_host_file(fixture.location()).is_none());
}

// A process that is not an engine host process refuses `host stop`.
#[gpui::test]
async fn a_stop_command_is_refused_outside_an_engine_host_process(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let local = EngineAccess::Local(fixture.repo.clone());
    let error = cx
        .update(|cx| {
            local.request(
                HostCommand::Host(crate::protocol::HostCtl::Stop {
                    at_job_boundary: false,
                }),
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("not an engine host process"));
}

// A connected client keeps the host from idling; once it goes, the idle time starts then.
#[gpui::test]
async fn a_connected_client_keeps_the_engine_host_running(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let exited = start_host_process(&fixture, cx);
    let client = fixture.connect(ClientKind::Cli, cx);
    wait_for(cx, |cx| host_connections(cx) == 1);

    cx.executor().advance_clock(IDLE_EXIT * 3);
    cx.run_until_parked();
    assert!(!exited.get(), "exited with a client attached");

    drop(client);
    wait_for(cx, |cx| host_connections(cx) == 0);
    // The poke of the client going was sent before the count dropped; the host hears it now,
    // so the idle time starts before the clock moves.
    cx.run_until_parked();
    cx.executor()
        .advance_clock(IDLE_EXIT - Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        !exited.get(),
        "the idle time counted from before the client left"
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(exited.get());
    assert!(read_host_file(fixture.location()).is_none());
}

// A second engine host of the same project is refused by the writer lock and told who runs.
#[gpui::test]
async fn a_second_engine_host_is_refused_by_the_writer_lock(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let _exited = start_host_process(&fixture, cx);
    let info = live_host(fixture.location()).expect("a live writer");

    let refused = open_host_writer(fixture.location(), &StateApplyHooks::new()).err();
    assert_eq!(
        refused.as_deref(),
        Some(
            format!(
                "an engine host for project-1 is already running (pid {}, started {})",
                info.pid,
                info.started.to_rfc3339()
            )
            .as_str()
        )
    );
    // The first host still holds the writer and its host.json.
    assert_eq!(live_host(fixture.location()), Some(info));
}

/// A spawner that starts nothing: it counts its calls and runs `on_spawn` in their place.
struct FakeSpawner {
    spawns: Arc<std::sync::atomic::AtomicUsize>,
    on_spawn: Box<dyn Fn() + Send + Sync>,
    exits: Option<String>,
}

struct FakeChild(Option<String>);

impl SpawnedHost for FakeChild {
    fn try_exit(&mut self) -> Option<String> {
        self.0.clone()
    }

    fn release(self: Box<Self>) {}
}

impl HostSpawner for FakeSpawner {
    fn spawn(&self, _: &str, _: &Path) -> anyhow::Result<Box<dyn SpawnedHost>> {
        self.spawns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.on_spawn)();
        Ok(Box::new(FakeChild(self.exits.clone())))
    }
}

fn ensure_options(
    fixture: &Fixture,
    on_spawn: Box<dyn Fn() + Send + Sync>,
    exits: Option<String>,
    timeout: Duration,
) -> (EnsureHost, Arc<std::sync::atomic::AtomicUsize>) {
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let options = EnsureHost {
        spawner: Arc::new(FakeSpawner {
            spawns: spawns.clone(),
            on_spawn,
            exits,
        }),
        log: fixture.root.path().join("logs").join("engine-host.log"),
        start_timeout: timeout,
        poll_interval: Duration::from_millis(1),
    };
    (options, spawns)
}

// With a live writer named in host.json, ensure_host attaches to it at once and starts nothing.
#[gpui::test]
async fn ensure_host_attaches_at_once_to_a_live_writer(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = fixture();
    let host = start_writer(&fixture, true, cx);
    let (options, spawns) = ensure_options(
        &fixture,
        Box::new(|| panic!("a host was started beside a live writer")),
        None,
        Duration::from_secs(60),
    );

    let attach = ensure_host(fixture.location(), "lanes: start", &options).unwrap();
    assert_eq!(
        attach,
        Attach {
            writer_pid: std::process::id(),
            socket: host.server().map(|server| server.socket().to_path_buf()),
            mode: HostMode::Window,
            spawned: false,
        }
    );
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 0);
    let client = HostClient::connect(
        &attach.socket.unwrap(),
        fixture.client_options(ClientKind::Cli),
        cx.executor(),
    )
    .unwrap();
    assert_eq!(client.welcomed().writer_pid, std::process::id());
}

// A host.json naming a dead pid, or this pid with another start time, is not live: ensure_host
// starts a host and attaches to whichever writer host.json names live afterwards.
#[test]
fn ensure_host_starts_a_host_past_a_dead_or_restarted_writer() {
    let fixture = fixture();
    let ours = HostInfo::this_process(HostMode::Host, None, VERSION, chrono::Utc::now());
    let restarted = HostInfo {
        start_time: Some(ours.start_time.unwrap_or(0) + 1),
        ..ours.clone()
    };
    let dead = HostInfo {
        pid: i32::MAX as u32 - 7,
        ..ours.clone()
    };
    for stale in [restarted, dead] {
        assert!(!stale.is_live());
        write_host_file(fixture.location(), &stale).unwrap();
        let location = fixture.location().clone();
        let live = ours.clone();
        let (options, spawns) = ensure_options(
            &fixture,
            Box::new(move || write_host_file(&location, &live).unwrap()),
            None,
            Duration::from_secs(60),
        );
        let attach = ensure_host(fixture.location(), "lanes: start", &options).unwrap();
        assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(attach.spawned);
        assert_eq!(attach.writer_pid, std::process::id());
        assert_eq!(attach.mode, HostMode::Host);
    }
}

// When no live writer appears in time, the error names the reason and the host's log.
#[test]
fn ensure_host_timeout_names_the_log() {
    let fixture = fixture();
    let (options, spawns) = ensure_options(
        &fixture,
        Box::new(|| {}),
        Some("exit status: 1".to_string()),
        Duration::from_millis(20),
    );
    let error = ensure_host(fixture.location(), "lanes: start", &options)
        .unwrap_err()
        .to_string();
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(error.contains("lanes: start"), "{error}");
    assert!(error.contains("exit status: 1"), "{error}");
    assert!(
        error.ends_with(&format!("see {}", options.log.display())),
        "{error}"
    );
    let log = std::fs::read_to_string(&options.log).unwrap();
    assert!(
        log.contains("starting the engine host: lanes: start"),
        "{log}"
    );
}

#[test]
fn the_engine_host_log_is_named_for_the_project() {
    let path = crate::host_log_path(Path::new("/logs"), "project-1");
    let hex = crate::project_hex("project-1");
    assert_eq!(hex.len(), 12);
    assert_eq!(path, PathBuf::from(format!("/logs/engine-host-{hex}.log")));
    assert_eq!(
        crate::socket_path(Path::new("/hosts"), "project-1"),
        PathBuf::from(format!("/hosts/{hex}.sock"))
    );
}

#[test]
fn a_stop_is_signaled_only_after_its_reply_was_flushed() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (server_side, client_side) = UnixStream::pair().unwrap();
    let peer = bongos_project_repo::peer_identity(&server_side, ClientKind::Cli).unwrap();
    let (outbox, lines) = std::sync::mpsc::sync_channel(crate::server::OUTBOX_LINES);
    let connection =
        crate::server::Connection::new(0, peer, server_side.try_clone().unwrap(), outbox);

    let written = Arc::new(AtomicBool::new(false));
    let stopped_early = Arc::new(AtomicBool::new(false));
    let (done, finished) = std::sync::mpsc::channel();
    let request = std::thread::spawn({
        let written = written.clone();
        let stopped_early = stopped_early.clone();
        move || {
            let signal = move |_at_job_boundary: bool| {
                if !written.load(Ordering::SeqCst) {
                    stopped_early.store(true, Ordering::SeqCst);
                }
            };
            crate::server::reply_then_stop(
                &connection,
                7,
                crate::protocol::HostResult::Ok(json!(null)),
                Some((signal, false)),
            );
            done.send(()).ok();
        }
    });

    // This test is the connection's writing thread, held until the reply is queued.
    let (line, flushed) = lines.recv_timeout(Duration::from_secs(30)).unwrap();
    let mut server_side = server_side;
    server_side.write_all(&line).unwrap();
    server_side.flush().unwrap();
    let mut reply = String::new();
    BufReader::new(client_side).read_line(&mut reply).unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(&reply).unwrap(),
        ServerMessage::Reply { id: 7, .. }
    ));
    written.store(true, Ordering::SeqCst);
    if let Some(flushed) = flushed {
        flushed.send(()).unwrap();
    }
    finished.recv_timeout(Duration::from_secs(30)).unwrap();
    request.join().unwrap();
    assert!(
        !stopped_early.load(Ordering::SeqCst),
        "the stop was signaled before the reply was written"
    );
}
