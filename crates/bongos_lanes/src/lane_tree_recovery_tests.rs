//! A lane tree a job wrote: a crashed pass leaves the tree's `.git` naming a private directory,
//! and a job may plant a `.git` of its own. Real git in a temporary directory. A canary file
//! shows whether anything the planted git directory names (`core.fsmonitor`, a hook) ran.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use bongos_confine::private_git::{MARKER, PrivateGit, RecoverOutcome, prepare, recover};
use bongos_workflow::builtin_merge::MergeStyle;
use bongos_workflow::worktrees::run_branch_name;
use gpui::TestAppContext;

use crate::job_trees::JobRepository;
use crate::lane_tree::{
    LaneMerge, LaneMergeError, LastEnd, MergeTarget, SaveTarget, merge_in_lane_tree, reset_tree,
};
use crate::lane_tree_tests::{Fixture, RUN_1, RUN_2, TASK_BRANCH, git, last, snapshot};

/// A shell script at `path` that appends a line to `canary`.
fn write_canary_script(path: &Path, canary: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("#!/bin/sh\necho ran >> '{}'\nexit 0\n", canary.display()),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Makes the git directory `dir` run a canary from `core.fsmonitor` and from the hooks git runs
/// when a branch moves or is checked out.
fn plant_canaries(dir: &Path, canary: &Path) {
    let monitor = dir.join("planted-fsmonitor");
    write_canary_script(&monitor, canary);
    let config = dir.join("config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!("\n[core]\n\tfsmonitor = {}\n", monitor.display()));
    std::fs::write(&config, text).unwrap();
    for hook in ["post-checkout", "reference-transaction", "post-commit"] {
        write_canary_script(&dir.join("hooks").join(hook), canary);
    }
}

/// The pid of a child process that has exited and been reaped.
#[allow(clippy::disallowed_methods)]
fn dead_pid() -> u32 {
    let mut child = Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A lane tree on its first run branch, with the git directory the repository records for it.
struct Lane {
    fixture: Fixture,
    tip: PathBuf,
    linked: PathBuf,
    scratch: PathBuf,
    canary: PathBuf,
    original_dot_git: Vec<u8>,
}

impl Lane {
    async fn new() -> Self {
        let fixture = Fixture::new();
        let tip = fixture.tip();
        let first = fixture.task_job(RUN_1, None);
        reset_tree(&fixture.git, &fixture.request(&first, false))
            .await
            .unwrap();
        let linked = std::fs::canonicalize(git(
            &tip,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ))
        .unwrap();
        let scratch = fixture.base.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let canary = fixture.base.join("canary.txt");
        let original_dot_git = std::fs::read(tip.join(".git")).unwrap();
        Self {
            fixture,
            tip,
            linked,
            scratch,
            canary,
            original_dot_git,
        }
    }

    /// The job's private git over the tree, taken by this process.
    async fn private(&self) -> PrivateGit {
        prepare(&self.tip, &self.linked, &self.scratch)
            .await
            .unwrap()
    }

    /// The private directory carries the canaries.
    fn plant_in_private_dir(&self) {
        let private_dir = self.scratch.join("git");
        assert!(private_dir.is_dir());
        plant_canaries(&private_dir, &self.canary);
        assert_eq!(
            std::fs::read_to_string(self.tip.join(".git")).unwrap(),
            format!("gitdir: {}\n", private_dir.display())
        );
    }

    /// The owner of the tree's marker dies without switching the tree back.
    fn kill_owner(&self) {
        let path = self.linked.join(MARKER);
        let text = std::fs::read_to_string(&path).unwrap();
        let dead = dead_pid();
        let text: String = text
            .lines()
            .map(|line| {
                if line.starts_with("pid ") {
                    format!("pid {dead}\n")
                } else if line.starts_with("start-time ") {
                    "start-time 1\n".to_string()
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        std::fs::write(&path, text).unwrap();
    }

    /// A crashed pass: private git over the tree with canaries planted, its owner gone.
    async fn crash(&self) {
        let private = self.private().await;
        self.plant_in_private_dir();
        std::mem::forget(private);
        self.kill_owner();
    }

    fn second_job(&self, end: LastEnd) -> JobRepository {
        self.fixture.task_job(RUN_2, Some(last(RUN_1, end)))
    }

    fn assert_no_canary(&self) {
        assert!(
            !self.canary.exists(),
            "something the planted git directory names ran: {}",
            std::fs::read_to_string(&self.canary).unwrap_or_default()
        );
    }

    fn assert_restored(&self) {
        assert_eq!(
            std::fs::read(self.tip.join(".git")).unwrap(),
            self.original_dot_git
        );
        assert!(!self.linked.join(MARKER).exists());
        assert!(!self.scratch.join("git").exists());
    }

    fn branches(&self) -> String {
        git(
            &self.fixture.app,
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        )
    }
}

// The control: the canaries are real, so git run by discovery in a tree whose `.git` names the
// private directory would run them.
#[gpui::test]
async fn the_planted_directory_runs_its_canaries_when_git_discovers_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    let private = lane.private().await;
    lane.plant_in_private_dir();
    lane.assert_no_canary();
    git(&lane.tip, &["status", "--porcelain"]);
    git(
        &lane.tip,
        &["commit", "-q", "--allow-empty", "-m", "planted"],
    );
    assert!(lane.canary.exists(), "git by discovery ran no canary");
    std::fs::remove_file(&lane.canary).unwrap();
    private.abandon().unwrap();
    lane.assert_restored();
}

// (a)
#[gpui::test]
async fn a_crashed_pass_is_recovered_before_the_reset_and_nothing_private_runs(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    lane.crash().await;
    let job = lane.second_job(LastEnd::Succeeded);

    let outcome = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    assert!(!outcome.added);
    assert_eq!(outcome.saved, None);
    assert_eq!(
        outcome.head,
        git(&lane.fixture.app, &["rev-parse", TASK_BRANCH])
    );
    lane.assert_no_canary();
    lane.assert_restored();
    assert_eq!(
        git(&lane.tip, &["branch", "--show-current"]),
        job.run_branch
    );
    assert_eq!(git(&lane.tip, &["status", "--porcelain"]), "");
    // The git run above is by discovery, in a tree whose `.git` is the original again.
    lane.assert_no_canary();

    // Recovery is idempotent: the next pass finds nothing to recover.
    assert_eq!(recover(&lane.linked).unwrap(), RecoverOutcome::NoMarker);
    assert_eq!(recover(&lane.linked).unwrap(), RecoverOutcome::NoMarker);
    let again = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    assert_eq!(again, outcome);
    lane.assert_no_canary();
}

// (b)
#[gpui::test]
async fn the_work_a_crashed_pass_left_is_saved_after_recovery(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    let first = run_branch_name(TASK_BRANCH, RUN_1, 1);
    let start = git(&lane.fixture.app, &["rev-parse", &first]);
    std::fs::write(lane.tip.join("README.md"), "half done\n").unwrap();
    std::fs::write(lane.tip.join("new.txt"), "new work\n").unwrap();
    lane.crash().await;
    let job = lane.second_job(LastEnd::Failed);

    let outcome = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    lane.assert_no_canary();
    lane.assert_restored();
    let saved = outcome.saved.expect("the work was saved");
    let Some((commit, SaveTarget::Branch(branch))) = saved.commit else {
        panic!("expected a save on the run branch: {saved:?}");
    };
    assert_eq!(branch, first);
    assert_eq!(git(&lane.fixture.app, &["rev-parse", &first]), commit);
    assert_eq!(
        git(&lane.fixture.app, &["rev-parse", &format!("{commit}^")]),
        start
    );
    assert_eq!(
        git(&lane.fixture.app, &["show", &format!("{commit}:README.md")]),
        "half done"
    );
    assert_eq!(
        git(&lane.fixture.app, &["show", &format!("{commit}:new.txt")]),
        "new work"
    );
    // Then the reset: the new run branch, the work gone from the tree.
    assert_eq!(
        git(&lane.tip, &["branch", "--show-current"]),
        job.run_branch
    );
    assert!(!lane.tip.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(lane.tip.join("README.md")).unwrap(),
        "readme\n"
    );
}

// (c)
#[gpui::test]
async fn a_tree_a_running_job_holds_is_neither_reset_nor_saved(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    std::fs::write(lane.tip.join("README.md"), "job work\n").unwrap();
    std::fs::write(lane.tip.join("new.txt"), "job file\n").unwrap();
    let private = lane.private().await;
    lane.plant_in_private_dir();
    let tree_before = snapshot(&lane.tip);
    let linked_before = snapshot(&lane.linked);
    let branches_before = lane.branches();
    let job = lane.second_job(LastEnd::Failed);

    let error = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap_err();
    assert!(error.contains("in use by a running job"), "{error}");
    assert!(error.contains("the lane waits"), "{error}");
    assert_eq!(snapshot(&lane.tip), tree_before);
    assert_eq!(snapshot(&lane.linked), linked_before);
    assert_eq!(lane.branches(), branches_before);
    assert!(!lane.branches().contains("refs/bongos/saved"));
    lane.assert_no_canary();

    // The same for a save alone.
    let error =
        crate::lane_tree::save_left_work(&lane.fixture.git, &lane.fixture.request(&job, false))
            .await
            .unwrap_err()
            .to_string();
    assert!(error.contains("in use by a running job"), "{error}");
    assert_eq!(snapshot(&lane.tip), tree_before);
    assert_eq!(lane.branches(), branches_before);

    // Once the job is switched back, the lane goes on and saves the work.
    private.abandon().unwrap();
    lane.assert_restored();
    let outcome = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    assert!(outcome.saved.is_some_and(|saved| saved.commit.is_some()));
    lane.assert_no_canary();
}

// (d)
#[gpui::test]
async fn a_clean_lane_tree_resets_as_before_and_leaves_no_marker(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    // No hook of the repository runs at a job start, and nothing else does either.
    write_canary_script(
        &lane.fixture.app.join(".git/hooks/post-checkout"),
        &lane.canary,
    );
    std::fs::write(lane.tip.join("scratch.txt"), "left behind\n").unwrap();
    std::fs::write(lane.tip.join("README.md"), "changed\n").unwrap();
    let job = lane.second_job(LastEnd::Succeeded);

    let outcome = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    assert!(!outcome.added);
    assert_eq!(outcome.saved, None);
    assert_eq!(
        outcome.head,
        git(&lane.fixture.app, &["rev-parse", TASK_BRANCH])
    );
    assert_eq!(
        git(&lane.tip, &["branch", "--show-current"]),
        job.run_branch
    );
    assert!(!lane.tip.join("scratch.txt").exists());
    assert_eq!(git(&lane.tip, &["status", "--porcelain"]), "");
    lane.assert_no_canary();
    lane.assert_restored();

    assert_eq!(recover(&lane.linked).unwrap(), RecoverOutcome::NoMarker);
    assert_eq!(recover(&lane.linked).unwrap(), RecoverOutcome::NoMarker);
    let again = reset_tree(&lane.fixture.git, &lane.fixture.request(&job, false))
        .await
        .unwrap();
    assert_eq!(again, outcome);
    lane.assert_no_canary();
    lane.assert_restored();
}

#[gpui::test]
async fn a_merge_in_a_tree_whose_git_was_planted_runs_nothing_from_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lane = Lane::new().await;
    let job = lane.fixture.task_job(RUN_1, None);
    let work = {
        std::fs::write(lane.tip.join("feature.txt"), "feature\n").unwrap();
        git(&lane.tip, &["add", "feature.txt"]);
        git(&lane.tip, &["commit", "-q", "-m", "feature"]);
        git(&lane.tip, &["rev-parse", "HEAD"])
    };

    // The job planted a git directory of its own and pointed the tree's `.git` at it.
    let hostile = lane.fixture.base.join("hostile");
    std::fs::create_dir_all(&hostile).unwrap();
    git(&hostile, &["init", "-q", "-b", "main"]);
    let hostile_git = hostile.join(".git");
    plant_canaries(&hostile_git, &lane.canary);
    let hostile_head = std::fs::read(hostile_git.join("HEAD")).unwrap();
    std::fs::write(
        lane.tip.join(".git"),
        format!("gitdir: {}\n", hostile_git.display()),
    )
    .unwrap();
    let branches = lane.branches();
    let tree_before = snapshot(&lane.tip);

    let request = LaneMerge {
        locations: lane.fixture.locations.clone(),
        repo: "app".to_string(),
        repository: lane.fixture.app.clone(),
        forbidden: lane.fixture.forbidden(),
        target: MergeTarget::MergeInto(Some("integration".to_string())),
        run_branch: job.run_branch.clone(),
        run_id: RUN_1.to_string(),
        workflow: "build".to_string(),
        todo: "ship it".to_string(),
        style: MergeStyle::FfOrMerge,
    };
    let error = merge_in_lane_tree(&lane.fixture.git, &request)
        .await
        .unwrap_err();
    assert!(matches!(error, LaneMergeError::NotOwnTree(_)), "{error:?}");
    lane.assert_no_canary();
    assert_eq!(
        std::fs::read(hostile_git.join("HEAD")).unwrap(),
        hostile_head
    );
    assert_eq!(snapshot(&lane.tip), tree_before);
    assert_eq!(lane.branches(), branches);
    assert_eq!(
        git(&lane.fixture.app, &["rev-parse", &job.run_branch]),
        work
    );

    // The same planted tree is not reset either.
    let error = reset_tree(
        &lane.fixture.git,
        &lane
            .fixture
            .request(&lane.second_job(LastEnd::Failed), false),
    )
    .await
    .unwrap_err();
    assert!(error.contains("does not name a linked worktree"), "{error}");
    assert!(error.ends_with("nothing was changed"), "{error}");
    lane.assert_no_canary();
    assert_eq!(snapshot(&lane.tip), tree_before);
    assert_eq!(lane.branches(), branches);
}
