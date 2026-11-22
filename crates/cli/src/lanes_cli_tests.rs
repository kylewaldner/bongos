use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};

use super::*;

type Replies = Rc<dyn Fn(&Value) -> Result<Value, RequestError>>;

struct FakeSession {
    sent: Rc<RefCell<Vec<Value>>>,
    replies: Replies,
}

impl Session for FakeSession {
    fn request(&self, command: Value) -> Result<Value, RequestError> {
        self.sent.borrow_mut().push(command.clone());
        (self.replies)(&command)
    }
}

/// A machine with fakes for the host, the writer and the app binary. `log` records every call
/// that reaches past the settings, in order.
struct Fake {
    enabled: bool,
    projects: Vec<ProjectRef>,
    cwd: PathBuf,
    writer: Option<WriterInfo>,
    start: Result<WriterInfo, StartFailure>,
    read: Result<ReadOutput, String>,
    connect_error: Option<RequestError>,
    replies: Replies,
    log: RefCell<Vec<String>>,
    sent: Rc<RefCell<Vec<Value>>>,
}

fn project(id: &str, folder: &str) -> ProjectRef {
    ProjectRef {
        id: id.to_string(),
        name: folder.rsplit('/').next().unwrap().to_string(),
        folder: PathBuf::from(folder),
        state: PathBuf::from(format!("/state/{id}")),
    }
}

fn host_writer() -> WriterInfo {
    WriterInfo {
        pid: 11,
        window: false,
        socket: Some(PathBuf::from("/hosts/p1.sock")),
    }
}

fn window_writer() -> WriterInfo {
    WriterInfo {
        pid: 22,
        window: true,
        socket: Some(PathBuf::from("/hosts/p1.sock")),
    }
}

impl Fake {
    fn new() -> Self {
        Self {
            enabled: true,
            projects: vec![project("p1", "/work/one")],
            cwd: PathBuf::from("/elsewhere"),
            writer: None,
            start: Ok(host_writer()),
            read: Ok(ReadOutput {
                code: 0,
                stdout: "read mode output\n".to_string(),
                stderr: String::new(),
            }),
            connect_error: None,
            replies: Rc::new(|command| Ok(json!({ "message": format!("ok {command}") }))),
            log: RefCell::new(Vec::new()),
            sent: Rc::default(),
        }
    }

    fn with_writer(mut self, writer: WriterInfo) -> Self {
        self.writer = Some(writer);
        self
    }

    fn replying(
        mut self,
        replies: impl Fn(&Value) -> Result<Value, RequestError> + 'static,
    ) -> Self {
        self.replies = Rc::new(replies);
        self
    }

    fn run(&self, words: &str) -> Outcome {
        let args: Vec<String> = words.split_whitespace().map(str::to_string).collect();
        execute(&args, self)
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    fn sent(&self) -> Vec<Value> {
        self.sent.borrow().clone()
    }
}

impl Env for Fake {
    fn lanes_enabled(&self) -> bool {
        self.enabled
    }

    fn projects(&self) -> Result<Vec<ProjectRef>, String> {
        Ok(self.projects.clone())
    }

    fn current_dir(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn writer(&self, _project: &ProjectRef) -> Option<WriterInfo> {
        self.writer.clone()
    }

    fn connect(&self, socket: &Path) -> Result<Box<dyn Session>, RequestError> {
        self.log
            .borrow_mut()
            .push(format!("connect {}", socket.display()));
        if let Some(error) = &self.connect_error {
            return Err(error.clone());
        }
        Ok(Box::new(FakeSession {
            sent: self.sent.clone(),
            replies: self.replies.clone(),
        }))
    }

    fn start_host(
        &self,
        project: &ProjectRef,
        reason: &str,
        hand_over: bool,
    ) -> Result<WriterInfo, StartFailure> {
        self.log.borrow_mut().push(format!(
            "start_host {} {reason} hand_over={hand_over}",
            project.id
        ));
        self.start.clone()
    }

    fn read_mode(&self, project: &ProjectRef, args: &[String]) -> Result<ReadOutput, String> {
        self.log
            .borrow_mut()
            .push(format!("read_mode {} {}", project.id, args.join(" ")));
        self.read.clone()
    }
}

fn lane(id: &str, state: &str, task: Option<&str>, reason: Option<&str>) -> Value {
    json!({
        "id": id,
        "kind": id.rsplit_once('-').unwrap().0,
        "state": state,
        "reason": reason,
        "task": task,
        "goal": task.map(|task| format!("goal of {task}")),
        "run": task.map(|_| "run-1"),
        "workflow": task.map(|_| "build"),
        "run_lifecycle": task.map(|_| "running"),
        "job_started": task.map(|_| "2026-01-01T00:00:00Z"),
        "jobs_done": 3,
    })
}

fn status_doc(split: bool) -> Value {
    let mut doc = json!({
        "schema": "lanes-status 1",
        "project": "p1",
        "header": {
            "dispatcher": "running", "lanes": 2, "running": 1, "idle": 1, "waiting": 0,
            "paused": 0, "stopped": 0, "halted": 0, "table_commit": "abc", "table_wait": null,
        },
        "lanes": [
            lane("build-1", "running", Some("t1"), split.then_some("waiting: master is split")),
            lane("build-2", "idle", None, None),
        ],
    });
    if split {
        doc["master_split"] = json!({ "change_id": "c1", "main": "m", "verified": "v" });
    }
    doc
}

fn status_replies(split: bool) -> impl Fn(&Value) -> Result<Value, RequestError> {
    move |_| Ok(json!({ "message": "text", "status": status_doc(split) }))
}

#[test]
fn lanes_start_with_no_writer_starts_a_host_then_sends_start() {
    let fake = Fake::new();
    let outcome = fake.run("lanes start --max-jobs 3 --unattended on");
    assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
    assert_eq!(
        fake.log(),
        vec![
            "start_host p1 bongos lanes start hand_over=false".to_string(),
            "connect /hosts/p1.sock".to_string()
        ]
    );
    let sent = fake.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["lanes"]["verb"], "start");
    assert_eq!(sent[0]["lanes"]["payload"]["max_jobs"], 3);
    assert_eq!(sent[0]["lanes"]["payload"]["unattended"], "on");
}

#[test]
fn lanes_start_with_a_window_writer_starts_a_host_that_takes_it_over() {
    let fake = Fake::new().with_writer(window_writer());
    let outcome = fake.run("lanes start");
    assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
    assert_eq!(
        fake.log()[0],
        "start_host p1 bongos lanes start hand_over=true"
    );
    assert_eq!(fake.sent().len(), 1);
}

#[test]
fn lanes_start_with_a_host_writer_attaches_without_starting_another() {
    let fake = Fake::new().with_writer(host_writer());
    assert_eq!(fake.run("lanes start").code, EXIT_DONE);
    assert_eq!(fake.log(), vec!["connect /hosts/p1.sock".to_string()]);
}

#[test]
fn status_json_carries_every_field_of_the_schema() {
    let fake = Fake::new()
        .with_writer(host_writer())
        .replying(status_replies(false));
    let outcome = fake.run("lanes status --json");
    assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
    let doc: Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert_eq!(doc["schema"], STATUS_SCHEMA);
    for key in [
        "dispatcher",
        "lanes",
        "running",
        "idle",
        "waiting",
        "paused",
        "stopped",
        "halted",
        "table_commit",
        "table_wait",
    ] {
        assert!(doc["header"].get(key).is_some(), "header lacks {key}");
    }
    for lane in doc["lanes"].as_array().unwrap() {
        for key in [
            "id",
            "kind",
            "state",
            "reason",
            "task",
            "goal",
            "run",
            "workflow",
            "run_lifecycle",
            "job_started",
            "jobs_done",
        ] {
            assert!(lane.get(key).is_some(), "lane lacks {key}");
        }
    }
    assert!(doc.get("master_split").is_none());
    assert_eq!(fake.sent()[0]["lanes"]["verb"], "status");
}

#[test]
fn a_split_master_shows_in_the_text_and_the_json_and_a_waiting_lane_keeps_its_task() {
    let fake = Fake::new()
        .with_writer(host_writer())
        .replying(status_replies(true));
    let text = fake.run("lanes status");
    assert_eq!(text.code, EXIT_DONE, "{text:?}");
    assert!(text.stdout.contains(
        "master is split: every landing waits until a person runs project: resolve split"
    ));
    let row = text
        .stdout
        .lines()
        .find(|line| line.starts_with("build-1"))
        .unwrap();
    assert!(row.contains("running"), "{row}");
    assert!(row.contains("t1"), "{row}");
    assert!(row.contains("waiting: master is split"), "{row}");

    let doc: Value = serde_json::from_str(&fake.run("lanes status --json").stdout).unwrap();
    assert_eq!(doc["master_split"]["change_id"], "c1");
    assert_eq!(doc["lanes"][0]["task"], "t1");
    assert_eq!(doc["lanes"][0]["reason"], "waiting: master is split");

    let clean = Fake::new()
        .with_writer(host_writer())
        .replying(status_replies(false));
    assert!(!clean.run("lanes status").stdout.contains("master is split"));
}

#[test]
fn a_status_of_another_schema_is_a_protocol_mismatch() {
    let fake = Fake::new()
        .with_writer(host_writer())
        .replying(|_| Ok(json!({ "status": { "schema": "lanes-status 2" } })));
    let outcome = fake.run("lanes status --json");
    assert_eq!(outcome.code, EXIT_PROTOCOL);
    assert!(outcome.stderr.contains("lanes-status 1"));
}

#[test]
fn dry_run_pick_and_run_reach_the_writer_and_print_its_answer() {
    let fake = Fake::new()
        .with_writer(host_writer())
        .replying(|command| Ok(json!({ "message": "answer", "echo": command })));
    let dry = fake.run("lanes dry-run");
    assert_eq!((dry.code, dry.stdout.as_str()), (EXIT_DONE, "answer\n"));
    let pick = fake.run("lanes pick --lane build-2 --json");
    assert_eq!(pick.code, EXIT_DONE);
    let doc: Value = serde_json::from_str(&pick.stdout).unwrap();
    assert_eq!(doc["echo"]["lanes"]["verb"], "pick");
    assert_eq!(doc["echo"]["lanes"]["payload"]["lane"], "build-2");
    let run = fake.run("run build --task t1 --queue");
    assert_eq!(run.code, EXIT_DONE, "{run:?}");
    let sent = fake.sent();
    let last = sent.last().unwrap();
    assert_eq!(last["workflow"]["verb"], "start");
    assert_eq!(last["workflow"]["payload"]["task_id"], "t1");
    assert_eq!(last["workflow"]["payload"]["workflow"], "build");
    assert_eq!(last["workflow"]["payload"]["queue"], true);
}

#[test]
fn run_with_no_writer_starts_a_host() {
    let fake = Fake::new();
    let outcome = fake.run("run build --task t1");
    assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
    assert_eq!(fake.log()[0], "start_host p1 bongos run hand_over=false");
    assert_eq!(fake.sent()[0]["workflow"]["verb"], "start");
}

#[test]
fn every_command_but_start_reaches_a_window_writer_and_starts_no_host() {
    for words in [
        "lanes status",
        "lanes status --json",
        "lanes dry-run",
        "lanes pick --lane build-1",
        "lanes stop",
        "lanes stop build-1",
        "lanes pause build-1",
        "lanes resume build-1",
        "run build --task t1",
        "host status",
        "host stop",
    ] {
        let fake = Fake::new()
            .with_writer(window_writer())
            .replying(|command| {
                Ok(if command["lanes"]["verb"] == "status" {
                    json!({ "status": status_doc(false) })
                } else {
                    json!({ "message": "done" })
                })
            });
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_DONE, "{words}: {outcome:?}");
        assert_eq!(
            fake.log(),
            vec!["connect /hosts/p1.sock".to_string()],
            "{words}"
        );
        assert_eq!(fake.sent().len(), 1, "{words}");
    }
}

#[test]
fn verbs_reach_the_writer_with_their_lane() {
    let fake = Fake::new().with_writer(host_writer());
    fake.run("lanes stop");
    fake.run("lanes pause build-2");
    fake.run("lanes resume build-2");
    fake.run("host stop --now");
    fake.run("host stop");
    let sent = fake.sent();
    assert_eq!(sent[0]["lanes"]["verb"], "stop");
    assert_eq!(sent[0]["lanes"]["payload"]["lane"], Value::Null);
    assert_eq!(sent[1]["lanes"]["verb"], "pause");
    assert_eq!(sent[1]["lanes"]["payload"]["lane"], "build-2");
    assert_eq!(sent[2]["lanes"]["verb"], "resume");
    assert_eq!(
        sent[3],
        json!({ "host": { "stop": { "at_job_boundary": false } } })
    );
    assert_eq!(
        sent[4],
        json!({ "host": { "stop": { "at_job_boundary": true } } })
    );
}

#[test]
fn with_no_writer_status_dry_run_and_pick_use_the_read_mode_and_start_nothing() {
    let fake = Fake::new();
    for (words, expected) in [
        ("lanes status", "read_mode p1 status"),
        ("lanes status --json", "read_mode p1 status --json"),
        ("lanes dry-run", "read_mode p1 dry-run"),
        (
            "lanes pick --lane build-1 --json",
            "read_mode p1 pick --json --lane build-1",
        ),
    ] {
        fake.log.borrow_mut().clear();
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_DONE, "{words}");
        assert_eq!(outcome.stdout, "read mode output\n", "{words}");
        assert_eq!(fake.log(), vec![expected.to_string()], "{words}");
    }
    assert!(fake.sent().is_empty());
}

#[test]
fn a_read_mode_exit_code_and_stderr_pass_through() {
    let mut fake = Fake::new();
    fake.read = Ok(ReadOutput {
        code: 2,
        stdout: String::new(),
        stderr: "no such read mode\n".to_string(),
    });
    let outcome = fake.run("lanes status");
    assert_eq!(
        (outcome.code, outcome.stderr.as_str()),
        (2, "no such read mode\n")
    );
    fake.read = Err("could not run the app".to_string());
    assert_eq!(fake.run("lanes status").code, EXIT_REFUSED);
}

#[test]
fn with_no_writer_stop_pause_resume_and_the_host_words_say_nothing_runs() {
    let fake = Fake::new();
    for (words, text) in [
        ("lanes stop", "no lanes are running"),
        ("lanes pause build-1", "no lanes are running"),
        ("lanes resume build-1", "no lanes are running"),
        ("host status", "no engine host runs for one"),
        ("host stop", "no engine host is running"),
    ] {
        let outcome = fake.run(words);
        assert_eq!(
            (outcome.code, outcome.stdout.trim()),
            (EXIT_DONE, text),
            "{words}"
        );
    }
    assert!(fake.log().is_empty());
}

#[test]
fn with_lanes_off_every_word_says_so_and_touches_nothing() {
    let mut fake = Fake::new();
    fake.enabled = false;
    for words in [
        "lanes start",
        "lanes status",
        "lanes dry-run",
        "lanes pick --lane build-1",
        "lanes stop",
        "run build --task t1",
        "host status",
    ] {
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_REFUSED, "{words}");
        assert!(outcome.stderr.contains("lanes are off"), "{words}");
        assert!(outcome.stdout.is_empty());
    }
    assert!(fake.log().is_empty());
}

#[test]
fn exit_code_one_is_a_refusal_of_the_writer() {
    let fake = Fake::new().with_writer(host_writer()).replying(|_| {
        Err(RequestError::Refused(
            "Lanes are off (lanes.enabled)".to_string(),
        ))
    });
    let outcome = fake.run("lanes pause build-1");
    assert_eq!(outcome.code, EXIT_REFUSED);
    assert_eq!(outcome.stderr, "Lanes are off (lanes.enabled)\n");
    assert!(outcome.stdout.is_empty());

    let mut unreachable = Fake::new().with_writer(host_writer());
    unreachable.connect_error = Some(RequestError::Unreachable("no socket".to_string()));
    assert_eq!(unreachable.run("lanes stop").code, EXIT_REFUSED);

    let mut no_socket = Fake::new().with_writer(WriterInfo {
        socket: None,
        ..window_writer()
    });
    let outcome = no_socket.run("lanes status");
    assert_eq!(outcome.code, EXIT_REFUSED);
    assert!(outcome.stderr.contains("serves no socket"));
    no_socket.writer = None;
}

#[test]
fn exit_code_two_is_usage_or_a_project_that_is_not_found() {
    let fake = Fake::new();
    for words in [
        "lanes",
        "lanes frobnicate",
        "lanes pause",
        "lanes pause nolane",
        "lanes pick --lane",
        "lanes status --bogus",
        "run build",
        "lanes status --project nope",
    ] {
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_USAGE, "{words}: {outcome:?}");
        assert!(!outcome.stderr.is_empty(), "{words}");
    }
    assert!(fake.log().is_empty());
    let mut none = Fake::new();
    none.projects.clear();
    assert_eq!(none.run("lanes status").code, EXIT_USAGE);
}

#[test]
fn help_exits_zero_on_stdout() {
    let outcome = Fake::new().run("lanes --help");
    assert_eq!(outcome.code, EXIT_DONE);
    assert!(outcome.stdout.contains("start"));
}

#[test]
fn exit_code_three_is_a_host_that_could_not_start_with_its_log() {
    let mut fake = Fake::new();
    fake.start = Err(StartFailure {
        message: "the engine host exited (exit status: 1)".to_string(),
        log: PathBuf::from("/logs/engine-host-abc.log"),
    });
    for words in ["lanes start", "run build --task t1"] {
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_HOST_START, "{words}");
        assert!(
            outcome.stderr.contains("/logs/engine-host-abc.log"),
            "{words}"
        );
        assert!(outcome.stderr.contains("exited"), "{words}");
    }
    assert!(fake.sent().is_empty());
}

#[test]
fn exit_code_four_is_a_protocol_or_version_mismatch() {
    let mut fake = Fake::new().with_writer(host_writer());
    fake.connect_error = Some(RequestError::Protocol(
        "the engine host runs bongos 9; stop it with bongos host stop when its jobs end"
            .to_string(),
    ));
    let outcome = fake.run("lanes status");
    assert_eq!(outcome.code, EXIT_PROTOCOL);
    assert!(outcome.stderr.contains("bongos host stop"));
}

#[test]
fn the_project_is_chosen_by_flag_folder_or_being_the_only_one() {
    let mut fake = Fake::new();
    fake.projects = vec![
        project("p1", "/work/one"),
        project("p2", "/work/two"),
        project("p3", "/work/two/inner"),
    ];
    fake.cwd = PathBuf::from("/work/two/inner/src");
    // The deepest folder holding the current directory.
    fake.run("lanes status");
    assert_eq!(fake.log(), vec!["read_mode p3 status".to_string()]);

    for (words, expected) in [
        ("lanes status --project p1", "read_mode p1 status"),
        ("lanes status --project two", "read_mode p2 status"),
        ("lanes status --project /work/one", "read_mode p1 status"),
    ] {
        fake.log.borrow_mut().clear();
        assert_eq!(fake.run(words).code, EXIT_DONE, "{words}");
        assert_eq!(fake.log(), vec![expected.to_string()], "{words}");
    }

    // Outside every folder with several projects: exit 2 listing them.
    fake.cwd = PathBuf::from("/tmp/elsewhere");
    let outcome = fake.run("lanes status");
    assert_eq!(outcome.code, EXIT_USAGE);
    for id in ["p1", "p2", "p3"] {
        assert!(outcome.stderr.contains(id), "{}", outcome.stderr);
    }

    // The only project is the default anywhere.
    fake.projects.truncate(1);
    fake.log.borrow_mut().clear();
    assert_eq!(fake.run("lanes status").code, EXIT_DONE);
    assert_eq!(fake.log(), vec!["read_mode p1 status".to_string()]);
}

#[test]
fn only_the_lanes_words_are_taken() {
    assert!(is_lanes_word(Some("lanes")));
    assert!(is_lanes_word(Some("run")));
    assert!(is_lanes_word(Some("host")));
    assert!(!is_lanes_word(Some("./lanes")));
    assert!(!is_lanes_word(Some("--version")));
    assert!(!is_lanes_word(None));
}

#[cfg(unix)]
mod socket {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use super::*;
    use crate::lanes_cli::RealEnv;

    /// Serves one connection of the project's socket: the hello, then each request answered
    /// with `answer(&command)`. Ends when the client closes.
    fn serve(listener: UnixListener, version: &'static str, answer: fn(&Value) -> Value) {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let hello: Value = serde_json::from_str(&line).unwrap();
            if hello["bongos_version"] != version {
                writeln!(writer, "{}", json!({ "message": "refused", "text": format!("the engine host runs bongos {version}; stop it with bongos host stop when its jobs end") })).unwrap();
                return;
            }
            assert_eq!(hello["client"], "cli");
            writeln!(writer, "{}", json!({ "message": "welcome", "protocol": 1, "bongos_version": version, "writer_pid": 1, "mode": "host", "generation": 0 })).unwrap();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                // An event first: the client skips lines that are not its reply.
                writeln!(
                    writer,
                    "{}",
                    json!({ "message": "event", "event": { "event": "lanes-changed" } })
                )
                .unwrap();
                let result = answer(&request["command"]);
                writeln!(
                    writer,
                    "{}",
                    json!({ "message": "reply", "id": request["id"], "result": result })
                )
                .unwrap();
            }
        });
    }

    #[test]
    fn a_request_over_the_real_socket_is_answered_and_a_refusal_is_exit_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("p1.sock");
        let listener = UnixListener::bind(&path).unwrap();
        serve(listener, "9.9.9", |command| {
            if command["lanes"]["verb"] == "pause" {
                json!({ "refused": "build-1 is not a lane" })
            } else {
                json!({ "ok": { "message": "Stopping at the job boundary" } })
            }
        });
        let env = FakeSocketEnv {
            inner: RealEnv {
                program: PathBuf::from("/nonexistent"),
                bongos_version: "9.9.9".to_string(),
            },
            socket: path,
        };
        let stop = execute(&["lanes".to_string(), "stop".to_string()], &env);
        assert_eq!(
            (stop.code, stop.stdout.as_str()),
            (EXIT_DONE, "Stopping at the job boundary\n")
        );
        let pause = execute(
            &[
                "lanes".to_string(),
                "pause".to_string(),
                "build-1".to_string(),
            ],
            &env,
        );
        assert_eq!(
            (pause.code, pause.stderr.as_str()),
            (EXIT_REFUSED, "build-1 is not a lane\n")
        );
    }

    #[test]
    fn a_version_the_host_does_not_run_is_exit_four() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("p1.sock");
        let listener = UnixListener::bind(&path).unwrap();
        serve(listener, "1.0.0", |_| json!({ "ok": null }));
        let env = FakeSocketEnv {
            inner: RealEnv {
                program: PathBuf::from("/nonexistent"),
                bongos_version: "2.0.0".to_string(),
            },
            socket: path,
        };
        let outcome = execute(&["lanes".to_string(), "stop".to_string()], &env);
        assert_eq!(outcome.code, EXIT_PROTOCOL);
        assert!(outcome.stderr.contains("bongos host stop"), "{outcome:?}");
    }

    /// The real connection with the machine's lookups replaced: a host writer on `socket`.
    struct FakeSocketEnv {
        inner: RealEnv,
        socket: PathBuf,
    }

    impl Env for FakeSocketEnv {
        fn lanes_enabled(&self) -> bool {
            true
        }
        fn projects(&self) -> Result<Vec<ProjectRef>, String> {
            Ok(vec![project("p1", "/work/one")])
        }
        fn current_dir(&self) -> PathBuf {
            PathBuf::from("/work/one")
        }
        fn writer(&self, _project: &ProjectRef) -> Option<WriterInfo> {
            Some(WriterInfo {
                pid: 1,
                window: false,
                socket: Some(self.socket.clone()),
            })
        }
        fn connect(&self, socket: &Path) -> Result<Box<dyn Session>, RequestError> {
            Ok(Box::new(crate::lanes_cli::real::SocketSession::connect(
                socket,
                &self.inner.bongos_version,
                Duration::from_millis(1),
            )?))
        }
        fn start_host(&self, _: &ProjectRef, _: &str, _: bool) -> Result<WriterInfo, StartFailure> {
            panic!("no host starts")
        }
        fn read_mode(&self, _: &ProjectRef, _: &[String]) -> Result<ReadOutput, String> {
            panic!("no read mode")
        }
    }
}
