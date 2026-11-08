//! `ChatRuntime` against the fake CLIs in `test-fixtures/`, run in a real PTY through
//! `Project::create_terminal_task`. Parking is allowed, so timers run in real time and "within one
//! second" is measured on the wall clock. Nothing here touches the real home directory: the fakes
//! log to a temporary file and write their session files under a temporary home.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bongos_chat_model::{
    Agent, AgentAdapter as _, Capabilities, ChatRecord, ClaudeAdapter, CodexAdapter, LaunchKind,
    TaskFacts, read_record, record_path, resolve, seed, write_record,
};
use bongos_work::{LiveModesRegistry, WorkDirs, WorkSettings, WorkStore, WorkStoreEvent};
use bongos_work_model::session_context;
use fs::RealFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use project::Project;
use remote::RemoteClient;
use settings::{ChatAgentContent, Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace};

use crate::{
    CLAUDE_SETTINGS_FILE, CONTEXT_FILE, ChatDirs, ChatHealth, ChatRuntime, ChatState, ChatStore,
    ChatsSettings, LaunchError, LaunchRequest, PLUGIN_DIR, SYSTEM_FILE,
};

/// What a test allows for a state change to reach the runtime.
pub(super) const WITHIN: Duration = Duration::from_secs(1);
/// How long a test waits for something that has no time promise (a process starting).
pub(super) const STARTUP: Duration = Duration::from_secs(10);

/// The human-only artifacts of the test task: path, description.
const SECRETS: [(&str, &str); 3] = [
    ("secret-root.md", "Root-level compensation numbers"),
    ("notes/salary-review.md", "Salary review notes"),
    ("prompts.txt", "The private prompts Kyle typed"),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-fixtures")
        .join(name)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A task with one shared file and the human-only files of `SECRETS`.
fn seed_task(dir: &Path, todos: bool) {
    let mut task = String::from(
        "schema = 1\nid = \"t1\"\ngoal = \"Backfill usage events\"\nkind = \"pr-ship\"\n\
         status = \"open\"\ncreated = 2020-01-01T10:00:00Z\n",
    );
    if todos {
        task.push_str(
            "\n[[todos]]\nname = \"implement\"\nstate = \"open\"\n\n\
             [[todos]]\nname = \"review\"\nstate = \"open\"\n",
        );
    }
    write(&dir.join("task.toml"), &task);
    write(&dir.join("docs/design.md"), "# design\n");
    let mut manifest = String::from(
        "schema = 1\n\n[[artifact]]\npath = \"docs/design.md\"\ndescription = \"The backfill \
         design\"\nvisibility = \"shared\"\n",
    );
    for (path, description) in SECRETS {
        write(&dir.join(path), "private\n");
        manifest.push_str(&format!(
            "\n[[artifact]]\npath = \"{path}\"\ndescription = \"{description}\"\n\
             visibility = \"human-only\"\n"
        ));
    }
    write(&dir.join("artifacts.toml"), &manifest);
}

fn claude_caps() -> Capabilities {
    let flags = "--session-id\n--resume\n--fork-session\n--name\n--append-system-prompt\n\
                 --plugin-dir\n--settings\n--model\n";
    ClaudeAdapter::new("claude").capabilities_from_help("2.1.319 (Claude Code)", flags)
}

fn codex_caps() -> Capabilities {
    let help = "-m, --model <model>\n-c, --config <key=value>\nresume\nfork\nlogin\n";
    CodexAdapter::new("codex").capabilities_from_help("codex-cli 0.46.0", help)
}

pub(super) struct Harness {
    _temp: TempDir,
    pub(super) base: PathBuf,
    /// The fake CLIs' log.
    pub(super) log: PathBuf,
    /// The home directory the runtime and fake Codex use.
    pub(super) home: PathBuf,
    pub(super) runtime_root: PathBuf,
    pub(super) work: Entity<WorkStore>,
    pub(super) store: Entity<ChatStore>,
    pub(super) types_root: PathBuf,
    /// The task directory chats are scoped to.
    pub(super) scope: PathBuf,
    /// The directory chats run in.
    pub(super) cwd: PathBuf,
    pub(super) app_state: Arc<AppState>,
    pub(super) project: Entity<Project>,
    pub(super) runtime: Entity<ChatRuntime>,
}

pub(super) fn set_chat_settings(cx: &mut TestAppContext, enabled: bool, idle_after_secs: u64) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
                let chats = content.chats.get_or_insert_default();
                chats.enabled = Some(enabled);
                chats.idle_after_secs = Some(idle_after_secs);
                let agents = chats.agents.get_or_insert_default();
                agents.claude = Some(ChatAgentContent {
                    command: Some(fixture("fake-claude").to_string_lossy().into_owned()),
                    extra_args: None,
                });
                agents.codex = Some(ChatAgentContent {
                    command: Some(fixture("fake-codex").to_string_lossy().into_owned()),
                    extra_args: None,
                });
            });
        });
    });
}

/// A runtime over temporary directories. `env` is added to the fakes' environment (through the
/// health's terminal environment, as a launch does).
pub(super) async fn harness(
    cx: &mut TestAppContext,
    enabled: bool,
    idle_after_secs: u64,
    env: &[(&str, &str)],
) -> Harness {
    harness_with(cx, enabled, idle_after_secs, env, false).await
}

/// `harness`, with todos on the test task when `todos` (none of them started).
async fn harness_with(
    cx: &mut TestAppContext,
    enabled: bool,
    idle_after_secs: u64,
    env: &[(&str, &str)],
    todos: bool,
) -> Harness {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = base.join("work");
    let scope = root.join("projects/invoicing/tasks/backfill");
    // The project the task is in, so the work tree holds it.
    write(
        &root.join("projects/invoicing/project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        &root.join("projects/invoicing/task.toml"),
        "schema = 1\nid = \"top\"\ngoal = \"Invoicing\"\nkind = \"other\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    seed_task(&scope, todos);
    let cwd = base.join("worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let types_root = base.join("chat-types");
    seed(&types_root).unwrap();
    write(
        &types_root.join("skills/model-routing/SKILL.md"),
        "---\nname: model-routing\ndescription: Route work\n---\nRoute work to models.\n",
    );
    let log = base.join("fake.log");
    let runtime_root = base.join("runtime");

    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        WorkSettings::register(cx);
        ChatsSettings::register(cx);
        app_state
    });
    set_chat_settings(cx, enabled, idle_after_secs);

    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let work = cx.new(|cx| WorkStore::new(fs, WorkDirs { root }, cx));
    let work_store = work.clone();
    let dirs = ChatDirs {
        types_root: types_root.clone(),
        runtime: runtime_root.clone(),
    };
    let store = cx.new(|cx| ChatStore::new(work, dirs, cx));
    let chat_store = store.clone();
    let mut health_env: BTreeMap<String, String> = env
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    health_env.insert("FAKE_CLI_LOG".into(), log.to_string_lossy().into_owned());
    health_env.insert("FAKE_CLI_HOME".into(), home.to_string_lossy().into_owned());
    let health = cx.new(|cx| ChatHealth::new(health_env, cx));
    cx.update(|cx| ChatHealth::set_global(health, cx));
    let runtime = cx.new(|_| ChatRuntime::new(store, home.clone()));
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    Harness {
        _temp: temp,
        base,
        log,
        home,
        runtime_root,
        work: work_store,
        store: chat_store,
        types_root,
        scope,
        cwd,
        app_state,
        project,
        runtime,
    }
}

impl Harness {
    /// A new chat of `agent` on the test task, its record written, and the request to launch it.
    pub(super) fn request(&self, agent: Agent, chat_type: &str) -> LaunchRequest {
        let record = ChatRecord::new(agent, chat_type, self.cwd.clone());
        let path = record_path(&self.scope, &record.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_record(&path, &record).unwrap();
        let resolved = resolve(
            chat_type,
            agent,
            &self.types_root,
            &self.home.join(".claude"),
        )
        .unwrap();
        LaunchRequest {
            record,
            record_path: path,
            scope_dir: self.scope.clone(),
            task_id: Some("t1".to_string()),
            kind: LaunchKind::New,
            context: session_context(&self.scope).unwrap(),
            facts: TaskFacts {
                project_title: Some("Invoicing".to_string()),
                project_dir: Some(self.base.join("work/projects/invoicing")),
                notes_dir: Some(self.scope.join("notes")),
                deps: vec![("Add the schema".to_string(), "done".to_string())],
                pr_links: vec![],
            },
            resolved,
            handoff: None,
            caps: Some(match agent {
                Agent::Claude => claude_caps(),
                Agent::Codex => codex_caps(),
            }),
            force: vec![],
        }
    }

    pub(super) async fn launch(
        &self,
        request: LaunchRequest,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<(), LaunchError> {
        let projects = vec![project.clone()];
        let task = self
            .runtime
            .update(cx, |runtime, cx| runtime.launch(request, &projects, cx));
        task.await
    }

    /// Launches `agent` and waits for the fake to be ready for input. Returns the chat id.
    pub(super) async fn start(&self, agent: Agent, cx: &mut TestAppContext) -> String {
        let request = self.request(agent, "implement");
        let id = request.record.id.clone();
        self.launch(request, &self.project, cx).await.unwrap();
        let name = agent.as_str();
        wait_for(cx, "the fake to start", STARTUP, |_| {
            self.log_lines(name).iter().any(|line| line == "start")
        })
        .await;
        self.wait_content(&id, "ready", cx).await;
        id
    }

    /// The fake's log lines of `name` (`claude` or `codex`), without the prefix.
    pub(super) fn log_lines(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name} ");
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// The values of the log lines `<name> <key> <value>`.
    fn logged(&self, name: &str, key: &str) -> Vec<String> {
        let prefix = format!("{key} ");
        self.log_lines(name)
            .iter()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    pub(super) fn state(&self, id: &str, cx: &mut TestAppContext) -> ChatState {
        self.runtime.read_with(cx, |runtime, _| runtime.state(id))
    }

    /// Types `line` and Enter into the chat's terminal.
    pub(super) fn type_line(&self, id: &str, line: &str, cx: &mut TestAppContext) {
        let text = format!("{line}\r");
        self.runtime.update(cx, |runtime, cx| {
            let terminal = runtime.live(id).unwrap().terminal().clone();
            terminal.update(cx, |terminal, _| terminal.input(text.into_bytes()));
        });
    }

    /// Waits up to [`WITHIN`] for the chat to reach `expected`.
    async fn wait_state(&self, id: &str, expected: ChatState, cx: &mut TestAppContext) {
        if try_wait_for(cx, WITHIN, |cx| self.state(id, cx) == expected).await {
            return;
        }
        let content = self.runtime.update(cx, |runtime, cx| {
            runtime
                .live(id)
                .map(|chat| chat.terminal().read(cx).get_content())
        });
        panic!(
            "the chat is {} after {WITHIN:?}, not {expected}\nlog: {:#?}\nterminal: {content:?}",
            self.state(id, cx),
            std::fs::read_to_string(&self.log).unwrap_or_default(),
        );
    }

    pub(super) async fn wait_content(&self, id: &str, text: &str, cx: &mut TestAppContext) {
        wait_for(cx, &format!("`{text}` in the terminal"), STARTUP, |cx| {
            self.runtime.update(cx, |runtime, cx| {
                runtime
                    .live(id)
                    .is_some_and(|chat| chat.terminal().read(cx).get_content().contains(text))
            })
        })
        .await;
    }
}

/// Waits up to `limit` for `check`.
pub(super) async fn wait_for(
    cx: &mut TestAppContext,
    what: &str,
    limit: Duration,
    check: impl Fn(&mut TestAppContext) -> bool,
) {
    if !try_wait_for(cx, limit, check).await {
        panic!("timed out after {limit:?} waiting for {what}");
    }
}

/// Waits up to `limit` for `check`; returns whether it held.
pub(super) async fn try_wait_for(
    cx: &mut TestAppContext,
    limit: Duration,
    check: impl Fn(&mut TestAppContext) -> bool,
) -> bool {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if check(cx) {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
}

/// Every file under `dir`, with its path.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Whether the process `pid` exists (and is not a zombie). Linux only; elsewhere it says yes.
fn process_alive(pid: u32) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .split(") ")
            .nth(1)
            .is_some_and(|rest| rest.starts_with('Z'))
    })
}

#[gpui::test]
async fn test_launch_runs_in_cwd_with_argv_and_env(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    let dir = h.runtime_root.join(&id);

    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    let session_id = record
        .session_id
        .clone()
        .expect("a Claude launch sets the session id");
    assert!(record.context_digest.is_some());
    assert!(record.context_given.is_some());

    assert_eq!(h.logged("claude", "cwd"), vec![h.cwd.to_string_lossy()]);
    let args = h.logged("claude", "arg");
    let flag = |name: &str| {
        let at = args.iter().position(|arg| arg == name);
        at.map(|at| args[at + 1].clone())
    };
    assert_eq!(flag("--session-id"), Some(session_id));
    assert_eq!(
        flag("--append-system-prompt-file"),
        Some(dir.join(SYSTEM_FILE).to_string_lossy().into_owned())
    );
    assert_eq!(
        flag("--settings"),
        Some(
            dir.join(CLAUDE_SETTINGS_FILE)
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        flag("--plugin-dir"),
        Some(dir.join(PLUGIN_DIR).to_string_lossy().into_owned())
    );
    assert!(args.contains(&"--name".to_string()));

    let env: BTreeMap<String, String> = h
        .logged("claude", "env")
        .iter()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let text = |path: &Path| path.to_string_lossy().into_owned();
    assert_eq!(env.get("BONGOS_CHAT_ID"), Some(&id));
    assert_eq!(
        env.get("BONGOS_CHAT_TYPE").map(String::as_str),
        Some("implement")
    );
    assert_eq!(
        env.get("BONGOS_CHAT_AGENT").map(String::as_str),
        Some("claude")
    );
    assert_eq!(
        env.get("BONGOS_CHAT_RECORD"),
        Some(&text(&record_path(&h.scope, &id)))
    );
    assert_eq!(
        env.get("BONGOS_CHAT_CONTEXT"),
        Some(&text(&dir.join(CONTEXT_FILE)))
    );
    assert_eq!(env.get("BONGOS_TASK_ID").map(String::as_str), Some("t1"));
    assert_eq!(env.get("BONGOS_TASK_DIR"), Some(&text(&h.scope)));
    assert_eq!(env.get("BONGOS_WORKTREE"), Some(&text(&h.cwd)));
    assert_eq!(env.get("BONGOS_ROOT"), Some(&text(&h.home.join(".bongos"))));
    // Every other `BONGOS_*` variable is inherited from this test process, not set by the launch.
    for (name, value) in &env {
        if !bongos_chat_model::ENV_NAMES.contains(&name.as_str()) {
            assert_eq!(std::env::var(name).ok().as_ref(), Some(value), "{name}");
        }
    }
    assert_eq!(h.state(&id, cx), ChatState::Running);
}

#[gpui::test]
async fn test_runtime_files_are_private_and_regenerated(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let request = h.request(Agent::Claude, "implement");
    let id = request.record.id.clone();
    let dir = h.runtime_root.join(&id);
    // A file an earlier launch left behind is not kept: the directory is regenerated. The root
    // it sits in is private, as an earlier launch made it.
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(&h.runtime_root).unwrap();
        std::fs::set_permissions(&h.runtime_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    write(&dir.join("stale.md"), "old\n");
    h.launch(request, &h.project, cx).await.unwrap();

    assert_eq!(mode(&h.runtime_root), 0o700);
    assert_eq!(mode(&dir), 0o700);
    assert!(!dir.join("stale.md").exists());
    let files = files_under(&dir);
    for name in [CONTEXT_FILE, SYSTEM_FILE, CLAUDE_SETTINGS_FILE] {
        assert!(files.contains(&dir.join(name)), "{name} missing");
    }
    assert!(files.contains(&dir.join(PLUGIN_DIR).join("skills/model-routing/SKILL.md")));
    for file in &files {
        assert_eq!(mode(file), 0o600, "{}", file.display());
        let mut parent = file.parent().unwrap();
        while parent != dir {
            assert_eq!(mode(parent), 0o700, "{}", parent.display());
            parent = parent.parent().unwrap();
        }
    }
    let settings = std::fs::read_to_string(dir.join(CLAUDE_SETTINGS_FILE)).unwrap();
    assert!(settings.contains(&dir.join(CONTEXT_FILE).to_string_lossy().into_owned()));
}

#[gpui::test]
async fn test_generated_files_hold_no_human_only_path(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let claude = h.start(Agent::Claude, cx).await;
    let codex = h.start(Agent::Codex, cx).await;

    let mut outputs: Vec<String> = Vec::new();
    for id in [&claude, &codex] {
        for file in files_under(&h.runtime_root.join(id)) {
            outputs.push(std::fs::read_to_string(file).unwrap());
        }
    }
    for name in ["claude", "codex"] {
        outputs.extend(h.logged(name, "arg"));
        outputs.extend(h.logged(name, "env"));
    }
    let context = std::fs::read_to_string(h.runtime_root.join(&claude).join(CONTEXT_FILE)).unwrap();
    assert!(
        context.contains("docs/design.md"),
        "the shared file is listed"
    );
    for output in &outputs {
        for (path, description) in SECRETS {
            let file_name = Path::new(path).file_name().unwrap().to_str().unwrap();
            for needle in [path, file_name, description] {
                assert!(!output.contains(needle), "{needle} found in:\n{output}");
            }
        }
    }
    let given = h.runtime.read_with(cx, |runtime, _| {
        runtime.live(&claude).unwrap().given().to_vec()
    });
    assert_eq!(given.len(), 1);
    assert!(given[0].path.ends_with("docs/design.md"));
}

#[gpui::test]
async fn test_bell_title_and_exit_reach_the_state_within_a_second(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[("FAKE_CLI_TITLE", "Fixing the backfill")]).await;
    let id = h.start(Agent::Claude, cx).await;
    let title = |cx: &mut TestAppContext| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title().map(str::to_string)
        })
    };
    wait_for(cx, "the first title", WITHIN, |cx| {
        title(cx).as_deref() == Some("Fixing the backfill")
    })
    .await;

    h.type_line(&id, "title Writing tests", cx);
    wait_for(cx, "the new title", WITHIN, |cx| {
        title(cx).as_deref() == Some("Writing tests")
    })
    .await;
    assert!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.live(&id).unwrap().prompted()),
        "typing marks the chat prompted"
    );

    h.type_line(&id, "bell", cx);
    h.wait_state(&id, ChatState::Waiting, cx).await;
    // The output that follows a bell in the same read does not end the wait.
    cx.executor().timer(Duration::from_millis(700)).await;
    cx.run_until_parked();
    assert_eq!(h.state(&id, cx), ChatState::Waiting);

    h.type_line(&id, "hello", cx);
    h.wait_state(&id, ChatState::Running, cx).await;

    h.type_line(&id, "exit 3", cx);
    h.wait_state(&id, ChatState::Exited { code: Some(3) }, cx)
        .await;
    assert_eq!(h.state(&id, cx).to_string(), "exited (code 3)");
    // The exit writes `last_active` and `prompted` at once.
    wait_for(cx, "the record to say prompted", STARTUP, |_| {
        read_record(&record_path(&h.scope, &id)).unwrap().prompted
    })
    .await;
}

#[gpui::test]
async fn test_no_output_makes_a_chat_idle(cx: &mut TestAppContext) {
    let h = harness(cx, true, 1, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "idle", Duration::from_secs(3), |cx| {
        h.state(&id, cx) == ChatState::Idle
    })
    .await;
    h.type_line(&id, "hello", cx);
    h.wait_state(&id, ChatState::Running, cx).await;
}

#[gpui::test]
async fn test_kill_ends_the_process_and_close_drops_the_chat(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    let pid = h.runtime.update(cx, |runtime, cx| {
        runtime
            .live(&id)
            .unwrap()
            .terminal()
            .read(cx)
            .pid()
            .unwrap()
    });
    assert!(process_alive(pid.as_u32()));

    assert!(h.runtime.update(cx, |runtime, cx| runtime.kill(&id, cx)));
    wait_for(cx, "the exit after the kill", WITHIN, |cx| {
        matches!(h.state(&id, cx), ChatState::Exited { .. })
    })
    .await;
    if cfg!(target_os = "linux") {
        wait_for(cx, "the process to be gone", WITHIN, |_| {
            !process_alive(pid.as_u32())
        })
        .await;
    }

    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    assert!(!h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
}

#[gpui::test]
async fn test_chat_survives_closing_the_workspace_that_started_it(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let project = Project::test(h.app_state.fs.clone(), [], cx).await;
    let window = cx.add_window({
        let project = project.clone();
        move |window, cx| MultiWorkspace::test_new(project, window, cx)
    });
    let request = h.request(Agent::Claude, "implement");
    let id = request.record.id.clone();
    h.launch(request, &project, cx).await.unwrap();
    h.wait_content(&id, "ready", cx).await;

    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    let weak_project = project.downgrade();
    drop(project);
    cx.run_until_parked();
    assert!(
        weak_project.upgrade().is_none(),
        "the workspace and its project are gone"
    );

    h.type_line(&id, "bell", cx);
    h.wait_state(&id, ChatState::Waiting, cx).await;
    h.type_line(&id, "exit 0", cx);
    h.wait_state(&id, ChatState::Exited { code: Some(0) }, cx)
        .await;
}

#[gpui::test]
async fn test_a_remote_project_is_refused(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
    let ping_handler = server_cx.new(|_| ());
    server_session.add_request_handler::<rpc::proto::Ping, _, _, _>(
        ping_handler.downgrade(),
        |_entity, _envelope, _cx| async { Ok(rpc::proto::Ack {}) },
    );
    drop(connect_guard);
    let remote_client = RemoteClient::connect_mock(opts, cx).await;
    let params = h.app_state.clone();
    let remote = cx.update(|cx| {
        Project::remote(
            remote_client,
            params.client.clone(),
            params.node_runtime.clone(),
            params.user_store.clone(),
            params.languages.clone(),
            params.fs.clone(),
            false,
            cx,
        )
    });

    let request = h.request(Agent::Claude, "implement");
    let id = request.record.id.clone();
    let result = h.launch(request, &remote, cx).await;
    assert_eq!(result, Err(LaunchError::NoLocalProject));
    assert_eq!(
        LaunchError::NoLocalProject.to_string(),
        "Chats run locally; open a local project first"
    );
    cx.run_until_parked();
    assert!(!h.log_lines("claude").contains(&"start".to_string()));
    assert!(!h.runtime_root.join(&id).exists());
    assert_eq!(h.state(&id, cx), ChatState::Closed);

    // With a local project after it, the local one is used.
    let request = h.request(Agent::Claude, "implement");
    let projects = vec![remote, h.project.clone()];
    let task = h
        .runtime
        .update(cx, |runtime, cx| runtime.launch(request, &projects, cx));
    task.await.unwrap();
}

#[gpui::test]
async fn test_nothing_launches_while_chats_are_off(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    let request = h.request(Agent::Claude, "implement");
    let id = request.record.id.clone();
    assert_eq!(
        h.launch(request, &h.project, cx).await,
        Err(LaunchError::Off)
    );
    cx.run_until_parked();
    assert!(!h.runtime_root.join(&id).exists());
    assert!(h.log_lines("claude").is_empty());
}

#[gpui::test]
async fn test_last_active_writes_are_debounced(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let debounce = Duration::from_secs(2);
    h.runtime
        .update(cx, |runtime, _| runtime.set_active_debounce(debounce));
    let id = h.start(Agent::Claude, cx).await;
    // Let the launch's own write fall out of the debounce window.
    cx.executor().timer(debounce).await;
    let writes = |cx: &mut TestAppContext| {
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().active_writes)
    };
    let title = |cx: &mut TestAppContext| {
        h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().title().map(str::to_string)
        })
    };

    let before = writes(cx);
    for name in ["one", "two", "three", "four"] {
        h.type_line(&id, &format!("title {name}"), cx);
        wait_for(cx, "the title", WITHIN, |cx| {
            title(cx).as_deref() == Some(name)
        })
        .await;
    }
    let during = writes(cx);
    assert!(
        during - before <= 1,
        "four changes inside the debounce wrote {} times",
        during - before
    );
    cx.executor()
        .timer(debounce + Duration::from_millis(500))
        .await;
    cx.run_until_parked();
    let after = writes(cx);
    assert!(after - before <= 2, "{} writes", after - before);
    assert!(after > before, "the changes were written");
}

#[gpui::test]
async fn test_codex_session_id_is_captured(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Codex, cx).await;
    wait_for(cx, "the fake session file", STARTUP, |_| {
        !h.logged("codex", "session").is_empty()
    })
    .await;
    let session = h.logged("codex", "session")[0].clone();
    wait_for(cx, "the captured id in the record", STARTUP, |_| {
        read_record(&record_path(&h.scope, &id))
            .unwrap()
            .session_id
            .as_deref()
            == Some(session.as_str())
    })
    .await;
    let live = h.runtime.read_with(cx, |runtime, _| {
        runtime.live(&id).unwrap().session_id().map(str::to_string)
    });
    assert_eq!(live.as_deref(), Some(session.as_str()));
    // Codex gets its instructions file through `-c`.
    let args = h.logged("codex", "arg");
    assert!(
        args.iter()
            .any(|arg| arg.starts_with("model_instructions_file="))
    );
}

#[gpui::test]
async fn test_codex_launches_in_one_directory_wait_for_the_capture(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let first = h.request(Agent::Codex, "free");
    let second = h.request(Agent::Codex, "free");
    let ids = [first.record.id.clone(), second.record.id.clone()];
    let projects = vec![h.project.clone()];
    let (first, second) = h.runtime.update(cx, |runtime, cx| {
        (
            runtime.launch(first, &projects, cx),
            runtime.launch(second, &projects, cx),
        )
    });
    first.await.unwrap();
    second.await.unwrap();

    let session_ids = h.logged("codex", "session");
    assert_eq!(session_ids.len(), 2);
    // The second CLI started only after the first one's session file was there and claimed.
    let lines = h.log_lines("codex");
    let first_session = lines
        .iter()
        .position(|line| line.starts_with("session "))
        .unwrap();
    let second_start = lines.iter().rposition(|line| line == "start").unwrap();
    assert!(first_session < second_start, "{lines:#?}");
    wait_for(cx, "both ids in the records", STARTUP, |_| {
        ids.iter().all(|id| {
            read_record(&record_path(&h.scope, id))
                .unwrap()
                .session_id
                .is_some()
        })
    })
    .await;
    let recorded: Vec<String> = ids
        .iter()
        .map(|id| {
            read_record(&record_path(&h.scope, id))
                .unwrap()
                .session_id
                .unwrap()
        })
        .collect();
    assert_eq!(recorded, session_ids);
}

/// The `started` times of the todos of `t1` in the Work snapshot.
fn todo_starts(h: &Harness, cx: &mut TestAppContext) -> Vec<Option<bongos_work_model::Timestamp>> {
    h.work.read_with(cx, |work, _| {
        let tree = work.snapshot().expect("the work tree is loaded");
        let task = tree.task_by_id("t1").expect("t1 is in the tree");
        task.file
            .as_ref()
            .unwrap()
            .todos
            .iter()
            .map(|todo| todo.started)
            .collect()
    })
}

pub(super) async fn wait_for_task(h: &Harness, cx: &mut TestAppContext) {
    wait_for(cx, "t1 in the work tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id("t1").is_some())
        })
    })
    .await;
}

#[gpui::test]
async fn test_the_work_store_reports_the_open_chat_until_it_ends(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    wait_for_task(&h, cx).await;
    // The registry notifies the global work store.
    cx.update(|cx| {
        bongos_work::init(
            Arc::new(RealFs::new(None, cx.background_executor().clone())),
            WorkDirs {
                root: h.base.join("work"),
            },
            cx,
        );
        crate::live_modes::init(&h.runtime, &h.store, cx);
    });
    let changes = std::rc::Rc::new(std::cell::Cell::new(0usize));
    cx.update(|cx| {
        let changes = changes.clone();
        let global = WorkStore::global(cx).unwrap();
        cx.subscribe(&global, move |_, event, _| {
            if *event == WorkStoreEvent::LiveModesChanged {
                changes.set(changes.get() + 1);
            }
        })
        .detach();
    });
    let open_chats = |cx: &mut TestAppContext| {
        cx.update(|cx| LiveModesRegistry::live_modes("t1", cx).open_chats)
    };
    assert_eq!(open_chats(cx), 0);

    let id = h.start(Agent::Claude, cx).await;
    // The record is in the chat index once the work store has seen its file.
    wait_for(cx, "one open chat on t1", STARTUP, |cx| open_chats(cx) == 1).await;
    assert_eq!(
        cx.update(|cx| LiveModesRegistry::live_modes("top", cx).open_chats),
        0
    );
    let after_start = changes.get();
    assert!(after_start >= 1, "the registry was told the chat started");

    h.type_line(&id, "bell", cx);
    h.wait_state(&id, ChatState::Waiting, cx).await;
    assert_eq!(open_chats(cx), 1, "a waiting chat is still open");

    h.type_line(&id, "exit 0", cx);
    h.wait_state(&id, ChatState::Exited { code: Some(0) }, cx)
        .await;
    wait_for(cx, "no open chat on t1", WITHIN, |cx| open_chats(cx) == 0).await;
    assert!(
        changes.get() > after_start,
        "the registry was told the chat ended"
    );
}

#[gpui::test]
async fn test_a_chat_start_starts_the_current_todo_once(cx: &mut TestAppContext) {
    let h = harness_with(cx, true, 60, &[], true).await;
    wait_for_task(&h, cx).await;
    assert_eq!(todo_starts(&h, cx), vec![None, None]);

    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the current todo to start", STARTUP, |cx| {
        todo_starts(&h, cx)[0].is_some()
    })
    .await;
    let started = todo_starts(&h, cx);
    assert!(started[0].is_some());
    assert_eq!(started[1], None, "only the current todo starts");
    let history = |cx: &mut TestAppContext| {
        h.work.read_with(cx, |work, _| {
            let tree = work.snapshot().unwrap();
            let file = tree.task_by_id("t1").unwrap().file.clone().unwrap();
            file.history
                .iter()
                .filter(|entry| entry.event == "todo started")
                .map(|entry| entry.by.clone())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(history(cx), vec!["chat".to_string()]);

    // A second chat on the task leaves the time as it is.
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    h.start(Agent::Claude, cx).await;
    cx.run_until_parked();
    assert_eq!(todo_starts(&h, cx), started);
    assert_eq!(history(cx).len(), 1);

    // Asked directly, it says it did nothing.
    let again = cx.update(|cx| crate::start_current_todo(&h.work, "t1", cx));
    assert_eq!(again.await, Ok(false));
    assert_eq!(todo_starts(&h, cx), started);
}

#[gpui::test]
async fn test_start_current_todo_writes_once_and_ignores_other_tasks(cx: &mut TestAppContext) {
    let h = harness_with(cx, true, 60, &[], true).await;
    wait_for_task(&h, cx).await;
    // A group and an unknown task have no todo to start.
    for id in ["top", "nope"] {
        let task = cx.update(|cx| crate::start_current_todo(&h.work, id, cx));
        assert_eq!(task.await, Ok(false), "{id}");
    }
    assert_eq!(todo_starts(&h, cx), vec![None, None]);

    let first = cx.update(|cx| crate::start_current_todo(&h.work, "t1", cx));
    assert_eq!(first.await, Ok(true));
    let started = todo_starts(&h, cx);
    assert!(started[0].is_some() && started[1].is_none());
    let second = cx.update(|cx| crate::start_current_todo(&h.work, "t1", cx));
    assert_eq!(second.await, Ok(false));
    assert_eq!(todo_starts(&h, cx), started);
}

#[gpui::test]
async fn test_a_dropped_launch_task_frees_its_chat_id(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let request = h.request(Agent::Claude, "implement");
    let projects = vec![h.project.clone()];
    let task = h.runtime.update(cx, |runtime, cx| {
        runtime.launch(request.clone(), &projects, cx)
    });
    // While the task lives, the chat counts as launching.
    let again = h.runtime.update(cx, |runtime, cx| {
        runtime.launch(request.clone(), &projects, cx)
    });
    assert_eq!(again.await, Err(LaunchError::AlreadyRunning));
    drop(task);
    cx.run_until_parked();
    h.launch(request, &h.project, cx).await.unwrap();
}

#[cfg(unix)]
#[test]
fn test_runtime_root_is_created_private_and_checked() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use crate::launch::write_runtime_dir;

    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let files = vec![(PathBuf::from("context.md"), "text\n".to_string())];

    // Created with its missing parents; only the root and below are private.
    let root = base.join("deep/er/runtime");
    write_runtime_dir(&root, &root.join("c1"), &files).unwrap();
    assert_eq!(mode(&root), 0o700);
    write_runtime_dir(&root, &root.join("c1"), &files).unwrap();

    // A root anyone else can read is refused, not repaired, and nothing is written in it.
    let open = base.join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = write_runtime_dir(&open, &open.join("c1"), &files).unwrap_err();
    assert!(error.to_string().contains("0700"), "{error}");
    assert!(!open.join("c1").exists());
    assert_eq!(mode(&open), 0o755);

    // A symbolic link, even to a private directory, is refused.
    let target = base.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = base.join("link");
    symlink(&target, &link).unwrap();
    let error = write_runtime_dir(&link, &link.join("c1"), &files).unwrap_err();
    assert!(
        error.to_string().contains("not a real directory"),
        "{error}"
    );
    assert!(!target.join("c1").exists());

    // So is a file in its place.
    let file = base.join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(write_runtime_dir(&file, &file.join("c1"), &files).is_err());
}
