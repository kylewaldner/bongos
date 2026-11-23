//! Preparing a lane's locations: the verify, the overlap check and the floors come before
//! anything is made; then only components below existing roots are made, real git makes the
//! trees, and the `after_tree_created` hooks run once per new tree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use bongos_lanes_model::table::LaneTable;
use bongos_lanes_model::{LaneId, LaneRecord, LaneState, Problem, WaitReason};
use bongos_work_model::worktree_path::TemplateLayer;
use gpui::{App, AppContext as _, Task, TestAppContext};
use tempfile::TempDir;

use crate::create_tests::{listing, set_mode, writable};
use crate::hooks::{AFTER_TREE_CREATED, JobHook, JobHooks};
use crate::locations_tests::{BUILD_TABLE, FakeProfile, inputs, table};
use crate::prepare::{
    FreeSpace, MakeTree, PrepareRequest, PreparedLane, TreeRequest, prepare_lane, setup_marker,
    wait_lane,
};
use crate::profile::LaneProfileSource;

/// Runs real git in a test repository; blocking is fine in a test.
#[allow(clippy::disallowed_methods)]
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A machine in `temp`: the project folder, bongos's data directory and a repository `app`
/// with one commit.
struct Machine {
    temp: TempDir,
    made: Arc<Mutex<Vec<TreeRequest>>>,
    free_asked: Arc<Mutex<Vec<PathBuf>>>,
    free: u64,
}

impl Machine {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("project")).unwrap();
        std::fs::create_dir_all(temp.path().join("data")).unwrap();
        let app = temp.path().join("src/app");
        std::fs::create_dir_all(&app).unwrap();
        git(&app, &["init", "-q", "-b", "main"]);
        git(&app, &["commit", "-q", "--allow-empty", "-m", "start"]);
        Self {
            temp,
            made: Arc::default(),
            free_asked: Arc::default(),
            free: u64::MAX,
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn make_tree(&self) -> MakeTree {
        let made = self.made.clone();
        Arc::new(move |request: &TreeRequest| {
            made.lock().unwrap().push(request.clone());
            let path = request.path.to_string_lossy().into_owned();
            git(
                &request.repository_path,
                &["worktree", "add", "-q", "--detach", &path, "main"],
            );
            Ok(())
        })
    }

    fn free_space(&self) -> FreeSpace {
        let asked = self.free_asked.clone();
        let free = self.free;
        Arc::new(move |path: &Path| {
            asked.lock().unwrap().push(path.to_path_buf());
            Some(free)
        })
    }

    fn request(&self, table: LaneTable) -> PrepareRequest {
        PrepareRequest {
            inputs: inputs(self.path()),
            table: Arc::new(table),
            lane: LaneId::new("build", 1),
            live: vec![LaneId::new("build", 1), LaneId::new("build", 2)],
            make_tree: self.make_tree(),
            free_space: self.free_space(),
        }
    }

    fn made_count(&self) -> usize {
        self.made.lock().unwrap().len()
    }

    /// Everything on disk outside the repository, which git itself changes.
    fn disk(&self) -> Vec<PathBuf> {
        listing(self.path())
            .into_iter()
            .filter(|path| !path.starts_with("src"))
            .collect()
    }
}

#[derive(Default)]
struct FakeHook {
    calls: Mutex<Vec<(String, String, PathBuf)>>,
    fail: Mutex<Option<String>>,
}

impl JobHook for FakeHook {
    fn after_tree_created(
        &self,
        lane: &LaneId,
        repo: &str,
        path: &Path,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        self.calls
            .lock()
            .unwrap()
            .push((lane.to_string(), repo.to_string(), path.to_path_buf()));
        let fail = self.fail.lock().unwrap().clone();
        cx.background_spawn(async move {
            match fail {
                Some(message) => Err(message),
                None => Ok(()),
            }
        })
    }
}

async fn prepare(
    cx: &mut TestAppContext,
    request: PrepareRequest,
    profile: Option<Arc<FakeProfile>>,
) -> Result<PreparedLane, WaitReason> {
    let profile = profile.map(|profile| profile as Arc<dyn LaneProfileSource>);
    let task = cx.update(|cx| prepare_lane(request, profile, cx));
    task.await
}

fn waiting(reason: WaitReason) -> LaneRecord {
    let lane = wait_lane(
        &LaneRecord::new(&LaneId::new("build", 1), LaneState::Idle),
        reason,
    );
    assert_eq!(lane.state, LaneState::Waiting);
    lane
}

fn profile_named() -> FakeProfile {
    FakeProfile {
        name: Some("fast-disk".to_string()),
        ..FakeProfile::default()
    }
}

#[gpui::test]
async fn with_no_profile_the_defaults_are_made_without_verify_or_floor(cx: &mut TestAppContext) {
    let machine = Machine::new();
    let hook = Arc::new(FakeHook::default());
    cx.update(|cx| JobHooks::register(hook.clone(), cx));

    let prepared = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    let tip = machine.path().join("data/worktrees/app/lane-build-1");
    let second = machine.path().join("data/worktrees/app/lane-build-1-base");
    assert_eq!(
        prepared.created_trees,
        [
            ("app".to_string(), tip.clone()),
            ("app".to_string(), second.clone())
        ]
    );
    assert_eq!(git(&tip, &["rev-parse", "--is-inside-work-tree"]), "true");
    assert_eq!(
        git(&second, &["rev-parse", "--is-inside-work-tree"]),
        "true"
    );
    for name in ["target", "scratch", "target-base"] {
        assert!(
            machine
                .path()
                .join("project/lanes/build-1")
                .join(name)
                .is_dir(),
            "{name}"
        );
    }
    assert!(machine.made.lock().unwrap()[1].base);
    assert!(
        machine.free_asked.lock().unwrap().is_empty(),
        "no floor ran"
    );
    assert!(
        machine
            .disk()
            .iter()
            .all(|path| !path.to_string_lossy().contains(".bongos-probe")),
        "every probe file is removed"
    );
    assert_eq!(
        *hook.calls.lock().unwrap(),
        [
            ("build-1".to_string(), "app".to_string(), tip.clone()),
            ("build-1".to_string(), "app".to_string(), second),
        ]
    );

    // A second job start finds everything made: no tree is made and no hook runs again.
    let again = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    assert!(again.created_trees.is_empty());
    assert_eq!(machine.made_count(), 2);
    assert_eq!(hook.calls.lock().unwrap().len(), 2);
}

#[gpui::test]
async fn a_failing_verify_leaves_the_lane_waiting_before_anything_is_made(cx: &mut TestAppContext) {
    let machine = Machine::new();
    let before = machine.disk();
    let problem = Problem {
        requirement: "the drive of target is mounted".to_string(),
        message: "drive for target not mounted: /mnt/fast missing".to_string(),
        hint: None,
    };
    let profile = Arc::new(FakeProfile {
        verify_problems: Some(vec![problem]),
        floors: [("target".to_string(), 1)].into(),
        ..profile_named()
    });
    let reason = prepare(
        cx,
        machine.request(table(BUILD_TABLE)),
        Some(profile.clone()),
    )
    .await
    .unwrap_err();
    let lane = waiting(reason);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Environment(
            "drive for target not mounted: /mnt/fast missing".to_string()
        ))
    );
    assert_eq!(*profile.verified.lock().unwrap(), 1);
    assert!(profile.asked().is_empty(), "nothing was resolved");
    assert!(machine.free_asked.lock().unwrap().is_empty());
    assert_eq!(machine.made_count(), 0);
    assert_eq!(machine.disk(), before);
}

#[gpui::test]
async fn a_missing_root_creates_nothing_and_leaves_the_lane_waiting(cx: &mut TestAppContext) {
    let machine = Machine::new();
    let before = machine.disk();
    let unmounted = machine.path().join("unmounted");
    let profile =
        Arc::new(profile_named().at("build-1", "target", unmounted.join("build-1/target")));
    let reason = prepare(cx, machine.request(table(BUILD_TABLE)), Some(profile))
        .await
        .unwrap_err();
    let WaitReason::Environment(message) = waiting(reason).reason.unwrap() else {
        panic!("expected an environment wait");
    };
    let root = unmounted.join("build-1");
    assert_eq!(
        message,
        format!(
            "build-1 target: {} is missing (the root {} is set by project profile fast-disk); \
             nothing was created",
            root.display(),
            root.display()
        )
    );
    assert!(!unmounted.exists());
    assert_eq!(
        machine.made_count(),
        0,
        "not even the trees, whose roots exist"
    );
    assert_eq!(machine.disk(), before);
}

#[gpui::test]
async fn an_unwritable_profile_root_refuses_while_a_default_root_falls_back(
    cx: &mut TestAppContext,
) {
    let machine = Machine::new();
    let settings_root = machine.path().join("settings-root");
    let profile_root = machine.path().join("profile-root");
    for root in [&settings_root, &profile_root] {
        std::fs::create_dir(root).unwrap();
        set_mode(root, 0o555);
    }
    if writable(&profile_root) {
        set_mode(&settings_root, 0o755);
        set_mode(&profile_root, 0o755);
        eprintln!("skipped: the mode bits do not stop this user from writing");
        return;
    }
    let with_settings_root = |request: PrepareRequest| {
        let mut request = request;
        request.inputs.repositories[0].template = Some((
            format!("{}/{{slug}}", settings_root.display()),
            TemplateLayer::Default,
        ));
        request
    };
    let before = machine.disk();

    // The profile's root is meant: it refuses, and nothing is made, the fallback included.
    let profile = Arc::new(profile_named().at("build-1", "target", profile_root.join("target")));
    let reason = prepare(
        cx,
        with_settings_root(machine.request(table(BUILD_TABLE))),
        Some(profile),
    )
    .await
    .unwrap_err();
    let WaitReason::Environment(message) = waiting(reason).reason.unwrap() else {
        panic!("expected an environment wait");
    };
    assert!(
        message.starts_with(&format!(
            "build-1 target: {} is not writable",
            profile_root.display()
        )),
        "{message}"
    );
    assert!(
        message.ends_with(&format!(
            "(the root {} is set by project profile fast-disk); nothing was created",
            profile_root.display()
        )),
        "{message}"
    );
    assert_eq!(machine.disk(), before);
    assert_eq!(machine.made_count(), 0);

    // The user's default root falls back to the central directory.
    let prepared = prepare(
        cx,
        with_settings_root(machine.request(table(BUILD_TABLE))),
        None,
    )
    .await
    .unwrap();
    let tip = prepared.locations.get("tree:app").unwrap();
    assert_eq!(
        tip.path,
        machine.path().join("data/worktrees/app/lane-build-1")
    );
    assert_eq!(
        git(&tip.path, &["rev-parse", "--is-inside-work-tree"]),
        "true"
    );
    assert_eq!(listing(&settings_root), Vec::<PathBuf>::new());
    assert_eq!(listing(&profile_root), Vec::<PathBuf>::new());
    set_mode(&settings_root, 0o755);
    set_mode(&profile_root, 0o755);
}

#[gpui::test]
async fn a_failing_hook_leaves_the_lane_waiting_and_runs_again_until_it_succeeds(
    cx: &mut TestAppContext,
) {
    let machine = Machine::new();
    let hook = Arc::new(FakeHook {
        fail: Mutex::new(Some("setup script exited with 2".to_string())),
        ..FakeHook::default()
    });
    cx.update(|cx| JobHooks::register(hook.clone(), cx));
    let tip = machine.path().join("data/worktrees/app/lane-build-1");
    let second = machine.path().join("data/worktrees/app/lane-build-1-base");
    let both = [
        ("build-1".to_string(), "app".to_string(), tip.clone()),
        ("build-1".to_string(), "app".to_string(), second.clone()),
    ];

    let reason = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap_err();
    assert_eq!(
        waiting(reason).reason,
        Some(WaitReason::Hook {
            point: AFTER_TREE_CREATED.to_string(),
            message: "setup script exited with 2".to_string(),
        })
    );
    assert_eq!(
        *hook.calls.lock().unwrap(),
        both,
        "the first tree's failure does not skip the second tree's hook"
    );
    assert!(setup_marker(&tip).is_file());
    assert!(setup_marker(&second).is_file());

    // The next start finds both trees made but not set up: the hook runs again for both.
    let reason = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap_err();
    assert!(matches!(reason, WaitReason::Hook { .. }), "{reason:?}");
    assert_eq!(hook.calls.lock().unwrap()[2..], both);
    assert_eq!(machine.made_count(), 2, "no tree is made twice");

    // Once it succeeds the markers go, and later starts do not call it.
    *hook.fail.lock().unwrap() = None;
    let prepared = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    assert!(prepared.created_trees.is_empty());
    assert_eq!(hook.calls.lock().unwrap()[4..], both);
    assert!(!setup_marker(&tip).exists());
    assert!(!setup_marker(&second).exists());
    prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    assert_eq!(hook.calls.lock().unwrap().len(), 6);
}

#[gpui::test]
async fn a_tree_left_with_its_marker_by_a_crash_has_its_hook_run(cx: &mut TestAppContext) {
    let machine = Machine::new();
    prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    let tip = machine.path().join("data/worktrees/app/lane-build-1");
    assert!(!setup_marker(&tip).exists());

    // As if the app stopped between making the tree and running its hooks.
    std::fs::write(setup_marker(&tip), "").unwrap();
    let hook = Arc::new(FakeHook::default());
    cx.update(|cx| JobHooks::register(hook.clone(), cx));
    let prepared = prepare(cx, machine.request(table(BUILD_TABLE)), None)
        .await
        .unwrap();
    assert!(prepared.created_trees.is_empty());
    assert_eq!(
        *hook.calls.lock().unwrap(),
        [("build-1".to_string(), "app".to_string(), tip.clone())]
    );
    assert!(!setup_marker(&tip).exists());
}

#[gpui::test]
async fn a_disk_below_its_floor_waits_before_anything_is_made(cx: &mut TestAppContext) {
    let mut machine = Machine::new();
    machine.free = 1024 * 1024 * 1024;
    let before = machine.disk();
    let profile = Arc::new(FakeProfile {
        floors: [("target".to_string(), 10 * 1024 * 1024 * 1024)].into(),
        ..profile_named()
    });
    let reason = prepare(cx, machine.request(table(BUILD_TABLE)), Some(profile))
        .await
        .unwrap_err();
    assert_eq!(
        waiting(reason).reason,
        Some(WaitReason::Environment(
            "disk below floor on target: 1.0 GiB of 10.0 GiB".to_string()
        ))
    );
    assert_eq!(
        *machine.free_asked.lock().unwrap(),
        [machine.path().join("project/lanes/build-1/target")]
    );
    assert_eq!(machine.disk(), before);
    assert_eq!(machine.made_count(), 0);
}

#[gpui::test]
async fn a_lane_overlapping_a_live_lane_waits_naming_the_pair(cx: &mut TestAppContext) {
    let machine = Machine::new();
    let before = machine.disk();
    let shared = machine.path().join("project/shared");
    let profile = Arc::new(
        profile_named()
            .at("build-1", "target", shared.join("target"))
            .at("build-2", "target", shared.join("target/")),
    );
    let reason = prepare(cx, machine.request(table(BUILD_TABLE)), Some(profile))
        .await
        .unwrap_err();
    assert_eq!(
        waiting(reason).reason,
        Some(WaitReason::Environment(format!(
            "build-1 target and build-2 target resolve to {}",
            shared.join("target").display()
        )))
    );
    assert_eq!(machine.disk(), before);
}
