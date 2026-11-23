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
    work: bool,
    repository: bool,
    misses: RefCell<Vec<String>>,
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
            work: true,
            repository: true,
            misses: RefCell::new(Vec::new()),
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

    fn work_enabled(&self) -> bool {
        self.work
    }

    fn project_repository_enabled(&self) -> bool {
        self.repository
    }

    fn now(&self) -> String {
        NOW.to_string()
    }

    fn record_miss(&self, line: &str) -> Result<(), String> {
        self.misses.borrow_mut().push(line.to_string());
        Ok(())
    }
}

/// The pinned time of the fake machine.
const NOW: &str = "2026-02-03T04:05:06+00:00";

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

/// The hello's version must equal the app's built beside it. In a dev build the build scripts of
/// `cli` and the app rerun at different times, so a commit sha in it would differ from the app's
/// and every command would be refused (exit 4); such a build embeds none.
#[test]
fn a_dev_build_says_a_version_with_no_commit_sha() {
    let dev = option_env!("BONGOS_SKIP_GIT_HEAD_WATCH")
        .is_some_and(|value| !value.is_empty() && value != "0");
    if dev {
        assert_eq!(option_env!("ZED_COMMIT_SHA"), None);
    }
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
    for word in ["trigger", "queue", "pool", "profile"] {
        assert!(is_lanes_word(Some(word)), "{word}");
    }
    assert!(!is_lanes_word(Some("./lanes")));
    assert!(!is_lanes_word(Some("--version")));
    assert!(!is_lanes_word(None));
}

fn extension_of(command: &Value) -> (&str, &Value) {
    let extension = &command["extension"];
    (
        extension["namespace"].as_str().unwrap(),
        &extension["payload"],
    )
}

#[test]
fn trigger_queue_and_pool_reach_the_writer_as_extension_commands() {
    let fake = Fake::new().with_writer(host_writer());
    for (words, namespace, verb) in [
        ("trigger fire nightly", "trigger", "fire"),
        ("queue push build", "queue", "push"),
        ("pool status", "pool", "status"),
    ] {
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_DONE, "{words}: {outcome:?}");
        let sent = fake.sent();
        let (sent_namespace, payload) = extension_of(sent.last().unwrap());
        assert_eq!(
            (sent_namespace, payload["verb"].as_str()),
            (namespace, Some(verb))
        );
    }
    let sent = fake.sent();
    assert_eq!(extension_of(&sent[0]).1["name"], "nightly");
    assert_eq!(extension_of(&sent[0]).1["scheduled"], false);
    assert_eq!(fake.log(), vec!["connect /hosts/p1.sock".to_string(); 3]);
}

#[test]
fn the_stand_in_words_need_a_subcommand_and_a_name() {
    let fake = Fake::new();
    for words in [
        "trigger",
        "trigger fire",
        "queue",
        "queue push",
        "pool",
        "pool frob",
    ] {
        assert_eq!(fake.run(words).code, EXIT_USAGE, "{words}");
    }
    assert!(fake.log().is_empty());
}

#[test]
fn a_scheduled_firing_with_no_writer_starts_a_host_and_sends_to_it() {
    let fake = Fake::new();
    let outcome = fake.run("trigger fire nightly --scheduled");
    assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
    assert_eq!(
        fake.log(),
        vec![
            "start_host p1 bongos trigger fire hand_over=false".to_string(),
            "connect /hosts/p1.sock".to_string()
        ]
    );
    let sent = fake.sent();
    assert_eq!(sent.len(), 1);
    let (namespace, payload) = extension_of(&sent[0]);
    assert_eq!(namespace, "trigger");
    assert_eq!(payload["scheduled"], true);
    // A run started this way is a hand-started run: nothing names a lane.
    assert!(payload.get("lane").is_none());
    assert!(fake.misses.borrow().is_empty());

    let queued = Fake::new();
    assert_eq!(queued.run("queue push build --scheduled").code, EXIT_DONE);
    assert_eq!(
        queued.log()[0],
        "start_host p1 bongos queue push hand_over=false"
    );
}

#[test]
fn a_scheduled_firing_goes_to_a_window_or_a_host_that_holds_the_writer_and_starts_none() {
    for writer in [window_writer(), host_writer()] {
        for words in [
            "trigger fire nightly --scheduled",
            "queue push build --scheduled",
        ] {
            let fake = Fake::new().with_writer(writer.clone());
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
}

#[test]
fn a_scheduled_firing_with_lanes_off_names_the_setting_and_starts_nothing() {
    let mut fake = Fake::new();
    fake.enabled = false;
    for words in [
        "trigger fire nightly --scheduled",
        "queue push build --scheduled",
        "pool status",
    ] {
        let outcome = fake.run(words);
        assert_eq!(outcome.code, EXIT_REFUSED, "{words}");
        assert_eq!(
            outcome.stderr, "lanes are off (lanes.enabled); open the project in a window\n",
            "{words}"
        );
        assert!(outcome.stdout.is_empty());
    }
    assert!(fake.log().is_empty());
    assert!(fake.misses.borrow().is_empty());
}

#[test]
fn a_scheduled_firing_whose_host_cannot_start_is_exit_three_with_the_log() {
    let mut fake = Fake::new();
    fake.start = Err(StartFailure {
        message: "the engine host exited (exit status: 1)".to_string(),
        log: PathBuf::from("/logs/engine-host-abc.log"),
    });
    let outcome = fake.run("trigger fire nightly --scheduled");
    assert_eq!(outcome.code, EXIT_HOST_START);
    assert!(outcome.stderr.contains("/logs/engine-host-abc.log"));
    assert!(fake.sent().is_empty());
}

#[test]
fn a_scheduled_firing_for_a_project_not_on_this_machine_records_one_miss_and_is_exit_two() {
    let fake = Fake::new();
    let outcome = fake.run("trigger fire nightly --scheduled --project elsewhere");
    assert_eq!(outcome.code, EXIT_USAGE, "{outcome:?}");
    assert!(outcome.stderr.contains("elsewhere"));
    assert!(outcome.stderr.contains(MISSES_FILE));
    assert!(fake.log().is_empty());
    let misses = fake.misses.borrow();
    assert_eq!(misses.len(), 1);
    let line: Value = serde_json::from_str(&misses[0]).unwrap();
    assert_eq!(line["at"], NOW);
    assert_eq!(line["project"], "elsewhere");
    assert_eq!(line["trigger"], "nightly");
    assert!(line["reason"].as_str().unwrap().contains("elsewhere"));
    assert_eq!(line.as_object().unwrap().len(), 4);
    drop(misses);

    // A project that cannot be told apart is as missing, and the queue's push follows the rule.
    let mut none = Fake::new();
    none.projects.clear();
    assert_eq!(none.run("queue push build --scheduled").code, EXIT_USAGE);
    let line: Value = serde_json::from_str(&none.misses.borrow()[0]).unwrap();
    assert_eq!(line["project"], Value::Null);
    assert_eq!(line["trigger"], "build");

    // A firing by hand records nothing.
    let by_hand = Fake::new();
    assert_eq!(
        by_hand.run("trigger fire nightly --project elsewhere").code,
        EXIT_USAGE
    );
    assert!(by_hand.misses.borrow().is_empty());
}

#[test]
fn misses_are_appended_one_json_line_each_to_the_state_folder() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    append_miss(&state, r#"{"trigger":"a"}"#).unwrap();
    append_miss(&state, r#"{"trigger":"b"}"#).unwrap();
    let text = std::fs::read_to_string(state.join(MISSES_FILE)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, [r#"{"trigger":"a"}"#, r#"{"trigger":"b"}"#]);
    assert!(text.ends_with('\n'));
}

#[test]
fn profile_reads_with_lanes_off_whatever_holds_the_writer() {
    for writer in [None, Some(window_writer()), Some(host_writer())] {
        let mut fake = Fake::new();
        fake.writer = writer;
        fake.enabled = false;
        let outcome = fake.run("profile show --json --lane build-1");
        assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
        assert_eq!(outcome.stdout, "read mode output\n");
        assert_eq!(
            fake.log(),
            vec!["read_mode p1 profile show --json --lane build-1".to_string()]
        );
        assert!(fake.sent().is_empty());
    }
    let fake = Fake::new();
    assert_eq!(fake.run("profile").code, EXIT_DONE);
    assert_eq!(fake.log(), vec!["read_mode p1 profile".to_string()]);
}

#[test]
fn profile_needs_work_and_project_repositories_but_not_lanes() {
    let mut fake = Fake::new();
    fake.work = false;
    let outcome = fake.run("profile show");
    assert_eq!(outcome.code, EXIT_REFUSED);
    assert_eq!(outcome.stderr, "work is off (work.enabled)\n");
    fake.work = true;
    fake.repository = false;
    let outcome = fake.run("profile show");
    assert_eq!(outcome.code, EXIT_REFUSED);
    assert!(outcome.stderr.contains("project_repository.enabled"));
    assert!(fake.log().is_empty());
}

#[test]
fn an_unregistered_profile_read_exits_with_the_apps_code() {
    let mut fake = Fake::new();
    fake.read = Ok(ReadOutput {
        code: 2,
        stdout: String::new(),
        stderr: "no read mode profile is registered\n".to_string(),
    });
    let outcome = fake.run("profile");
    assert_eq!(
        (outcome.code, outcome.stderr.as_str()),
        (2, "no read mode profile is registered\n")
    );
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
            for stream in listener.incoming() {
                let stream = stream.unwrap();
                serve_connection(stream, version, answer);
            }
        });
    }

    fn serve_connection(
        stream: std::os::unix::net::UnixStream,
        version: &str,
        answer: fn(&Value) -> Value,
    ) {
        {
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
        }
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

    /// The machine of two timers that fire at once: the real start of a host with `program`, the
    /// real socket, the settings and the project list replaced. Both check for a writer at the
    /// same moment, so both start a host.
    #[cfg(target_os = "linux")]
    struct TimerEnv {
        project: ProjectRef,
        program: PathBuf,
        log: PathBuf,
        both_checked: std::sync::Barrier,
    }

    #[cfg(target_os = "linux")]
    impl Env for TimerEnv {
        fn lanes_enabled(&self) -> bool {
            true
        }
        fn projects(&self) -> Result<Vec<ProjectRef>, String> {
            Ok(vec![self.project.clone()])
        }
        fn current_dir(&self) -> PathBuf {
            self.project.folder.clone()
        }
        fn writer(&self, project: &ProjectRef) -> Option<WriterInfo> {
            self.both_checked.wait();
            crate::lanes_cli::real::live_writer(project)
        }
        fn connect(&self, socket: &Path) -> Result<Box<dyn Session>, RequestError> {
            Ok(Box::new(crate::lanes_cli::real::SocketSession::connect(
                socket,
                "9.9.9",
                Duration::from_millis(1),
            )?))
        }
        fn start_host(
            &self,
            project: &ProjectRef,
            reason: &str,
            hand_over: bool,
        ) -> Result<WriterInfo, StartFailure> {
            crate::lanes_cli::real::start_host_process(
                &self.program,
                project,
                reason,
                hand_over,
                &self.log,
                Duration::from_secs(60),
                // A hang guard: the loser's host exits at once, and the winner may take a while
                // to announce itself on a loaded machine.
                Duration::from_secs(60),
            )
        }
        fn read_mode(&self, _: &ProjectRef, _: &[String]) -> Result<ReadOutput, String> {
            panic!("no read mode")
        }
        fn work_enabled(&self) -> bool {
            true
        }
        fn project_repository_enabled(&self) -> bool {
            true
        }
        fn now(&self) -> String {
            NOW.to_string()
        }
        fn record_miss(&self, _: &str) -> Result<(), String> {
            panic!("no miss")
        }
    }

    /// A stand-in for the app binary: the first one started wins the lock, announces itself in
    /// `host.json` and stays until `alive` is removed; any other exits at once, as a host refused
    /// by the lock does. It reads its start time from `/proc`, so it runs on Linux only.
    #[cfg(target_os = "linux")]
    const HOST_SCRIPT: &str = r#"#!/bin/sh
echo "$$" >> "@DIR@/starts"
if mkdir "@DIR@/lock" 2>/dev/null; then
  echo "$$" >> "@DIR@/hosts"
  start=$(awk '{print $22}' /proc/$$/stat)
  printf '{"pid":%s,"start_time":%s,"socket":"@DIR@/p1.sock","mode":"host"}' "$$" "$start" > "@DIR@/state/host.json.tmp"
  mv "@DIR@/state/host.json.tmp" "@DIR@/state/host.json"
  while [ -e "@DIR@/alive" ]; do sleep 0.05; done
else
  exit 1
fi
"#;

    #[test]
    #[cfg(target_os = "linux")]
    fn two_scheduled_firings_with_no_writer_both_attach_to_one_host() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("alive"), "").unwrap();
        let script = root.join("host.sh");
        std::fs::write(
            &script,
            HOST_SCRIPT.replace("@DIR@", root.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let listener = UnixListener::bind(root.join("p1.sock")).unwrap();
        serve(
            listener,
            "9.9.9",
            |_| json!({ "ok": { "message": "fired" } }),
        );

        let env = TimerEnv {
            project: ProjectRef {
                id: "p1".to_string(),
                name: "one".to_string(),
                folder: root.join("work"),
                state: root.join("state"),
            },
            program: script,
            log: root.join("host.log"),
            both_checked: std::sync::Barrier::new(2),
        };
        let args: Vec<String> = [
            "trigger",
            "fire",
            "nightly",
            "--scheduled",
            "--project",
            "p1",
        ]
        .map(str::to_string)
        .into();
        let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
            let timers: Vec<_> = (0..2)
                .map(|_| scope.spawn(|| execute(&args, &env)))
                .collect();
            timers
                .into_iter()
                .map(|timer| timer.join().unwrap())
                .collect()
        });
        std::fs::remove_file(root.join("alive")).unwrap();

        for outcome in &outcomes {
            assert_eq!(outcome.code, EXIT_DONE, "{outcome:?}");
            assert_eq!(outcome.stdout, "fired\n");
        }
        let hosts = std::fs::read_to_string(root.join("hosts")).unwrap();
        assert_eq!(hosts.lines().count(), 1, "one host took the lock: {hosts}");
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
        fn work_enabled(&self) -> bool {
            true
        }
        fn project_repository_enabled(&self) -> bool {
            true
        }
        fn now(&self) -> String {
            NOW.to_string()
        }
        fn record_miss(&self, _: &str) -> Result<(), String> {
            panic!("no miss")
        }
    }
}
