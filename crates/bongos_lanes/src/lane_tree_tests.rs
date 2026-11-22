//! A lane's persistent tree over real git in a temporary directory: the reset at each job start,
//! the save before it, the merge in the tree, and the guards that keep every other checkout,
//! task worktree and lane tree untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use bongos_lanes_model::LaneId;
use bongos_workflow::builtin_merge::{MergeStyle, Merged};
use bongos_workflow::worktrees::{Git, run_branch_name};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::job_trees::{JobRepository, forbidden_for};
use crate::lane_tree::{
    Forbidden, LaneMerge, LaneMergeError, LaneMerged, LastEnd, LastRun, MergeTarget, ResetRequest,
    SaveTarget, check_own_tree, merge_in_lane_tree, reset_tree,
};
use crate::locations::{LaneLocations, LocationInputs, RepositoryPlace, binding_of, resolve_lane};
use crate::locations_tests::{FakeProfile, table};
use crate::profile::LaneProfileSource;

pub(crate) const RUN_1: &str = "01890a5d-ac96-774b-bcce-b30209000001";
pub(crate) const RUN_2: &str = "01890a5d-ac96-774b-bcce-b30209000002";
pub(crate) const RUN_3: &str = "01890a5d-ac96-774b-bcce-b30209000003";
pub(crate) const TASK_BRANCH: &str = "task/x";

pub(crate) const TABLE: &str = r#"schema = 1

[[kind]]
name = "build"
count = 2
[kind.binding]
dirs = ["target"]
trees = [{ repo = "app", mode = "persistent", base_tree = { dirs = ["target"] } }]
"#;

/// Runs real git in a test repository; blocking is fine in a test.
#[allow(clippy::disallowed_methods)]
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
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

/// Every file below `dir` with its bytes, links as their targets; `.git` directories included.
pub(crate) fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(dir).unwrap().to_path_buf();
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                files.insert(relative, target.to_string_lossy().as_bytes().to_vec());
            } else if metadata.is_dir() {
                pending.push(path);
            } else {
                files.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

pub(crate) struct Fixture {
    _temp: TempDir,
    pub(crate) base: PathBuf,
    pub(crate) git: Git,
    /// The primary checkout of `app`.
    pub(crate) app: PathBuf,
    /// The primary checkout of `lib`, a second repository.
    pub(crate) lib: PathBuf,
    /// A task worktree on [`TASK_BRANCH`], made by hand.
    pub(crate) task_worktree: PathBuf,
    /// Another checkout of the user's that no lane may touch.
    pub(crate) checkout: PathBuf,
    pub(crate) inputs: LocationInputs,
    pub(crate) locations: LaneLocations,
    pub(crate) all: Vec<LaneLocations>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::build(TABLE, |_| None)
    }

    /// The fixture with the project profile `profile` makes from the temporary directory.
    pub(crate) fn with_profile(profile: impl FnOnce(&Path) -> Option<FakeProfile>) -> Self {
        Self::build(TABLE, profile)
    }

    /// The fixture with the lane table `text` and the profile `profile` makes.
    pub(crate) fn build(text: &str, profile: impl FnOnce(&Path) -> Option<FakeProfile>) -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let profile = profile(&base);
        let profile = profile
            .as_ref()
            .map(|profile| profile as &dyn LaneProfileSource);
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(base.join("data")).unwrap();
        std::fs::create_dir_all(base.join("project")).unwrap();
        let runner = Git::new("git")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@localhost")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@localhost");

        let app = base.join("src/app");
        std::fs::create_dir_all(&app).unwrap();
        git(&app, &["init", "-q", "-b", "main"]);
        std::fs::write(app.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(app.join("README.md"), "readme\n").unwrap();
        git(&app, &["add", ".gitignore", "README.md"]);
        git(&app, &["commit", "-q", "-m", "first"]);
        for branch in ["release", "integration", "task/y"] {
            git(&app, &["branch", branch]);
        }
        git(&app, &["switch", "-q", "release"]);
        std::fs::write(app.join("RELEASE.md"), "release notes\n").unwrap();
        git(&app, &["add", "RELEASE.md"]);
        git(&app, &["commit", "-q", "-m", "release"]);
        git(&app, &["switch", "-q", "main"]);
        let task_worktree = base.join("tasks/x");
        git(
            &app,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                TASK_BRANCH,
                &task_worktree.to_string_lossy(),
            ],
        );
        std::fs::write(task_worktree.join("notes.txt"), "work in progress\n").unwrap();

        let lib = base.join("src/lib");
        std::fs::create_dir_all(&lib).unwrap();
        git(&lib, &["init", "-q", "-b", "main"]);
        std::fs::write(lib.join("lib.rs"), "// lib\n").unwrap();
        git(&lib, &["add", "lib.rs"]);
        git(&lib, &["commit", "-q", "-m", "lib"]);

        let checkout = base.join("src/checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q", "-b", "main"]);
        std::fs::write(checkout.join("mine.txt"), "mine\n").unwrap();
        git(&checkout, &["add", "mine.txt"]);
        git(&checkout, &["commit", "-q", "-m", "mine"]);
        std::fs::write(checkout.join("draft.txt"), "uncommitted\n").unwrap();

        let inputs = LocationInputs {
            project_id: "project-1".to_string(),
            project_folder: base.join("project"),
            project_repository: Some(base.join("project/repository")),
            home,
            central: base.join("data/worktrees"),
            repositories: vec![
                RepositoryPlace {
                    name: "app".to_string(),
                    path: app.clone(),
                    template: None,
                },
                RepositoryPlace {
                    name: "lib".to_string(),
                    path: lib.clone(),
                    template: None,
                },
                RepositoryPlace {
                    name: "checkout".to_string(),
                    path: checkout.clone(),
                    template: None,
                },
            ],
        };
        let table = table(text);
        let resolve = |index: u32| {
            let lane = LaneId::new("build", index);
            resolve_lane(
                &inputs,
                &table.locations,
                binding_of(&table, &lane),
                &lane,
                profile,
            )
            .unwrap()
        };
        let all = vec![resolve(1), resolve(2)];
        Self {
            _temp: temp,
            base,
            git: runner,
            app,
            lib,
            task_worktree,
            checkout,
            inputs,
            locations: all[0].clone(),
            all,
        }
    }

    pub(crate) fn tip(&self) -> PathBuf {
        self.locations.get("tree:app").unwrap().path.clone()
    }

    pub(crate) fn forbidden(&self) -> Forbidden {
        Forbidden {
            task_worktrees: vec![self.task_worktree.clone()],
            ..forbidden_for(
                &self.locations.lane,
                &self.inputs,
                &self.all,
                &bongos_work_model::store::WorkTree::default(),
            )
        }
    }

    pub(crate) fn task_job(&self, run_id: &str, last: Option<LastRun>) -> JobRepository {
        JobRepository::for_task("app", self.app.clone(), TASK_BRANCH, run_id, 1, last)
    }

    pub(crate) fn request(&self, job: &JobRepository, second: bool) -> ResetRequest {
        ResetRequest {
            locations: self.locations.clone(),
            repo: job.name.clone(),
            repository: job.repository.clone(),
            second,
            add_from: job.add_from.clone(),
            start: job.start.clone(),
            run_branch: job.run_branch.clone(),
            last: job.last.clone(),
            save_max_file_bytes: 1000,
            forbidden: self.forbidden(),
        }
    }

    async fn reset(&self, job: &JobRepository) -> Result<crate::lane_tree::ResetOutcome, String> {
        reset_tree(&self.git, &self.request(job, false)).await
    }

    /// The files of the task worktree and of the user's other checkout, byte for byte.
    pub(crate) fn untouched(&self) -> (BTreeMap<PathBuf, Vec<u8>>, BTreeMap<PathBuf, Vec<u8>>) {
        (snapshot(&self.task_worktree), snapshot(&self.checkout))
    }
}

pub(crate) fn last(run_id: &str, end: LastEnd) -> LastRun {
    LastRun {
        run_id: run_id.to_string(),
        run_branch: run_branch_name(TASK_BRANCH, run_id, 1),
        end,
    }
}

fn current_branch(tree: &Path) -> String {
    git(tree, &["branch", "--show-current"])
}

#[gpui::test]
async fn ignored_output_survives_two_jobs_and_nothing_else_is_touched(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let before = fixture.untouched();
    let main_checkout = git(&fixture.app, &["status", "--porcelain"]);

    // First use: the tree is added detached at the task branch, then put on the run branch.
    let first = fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    assert!(first.added);
    assert_eq!(first.saved, None);
    let task_head = git(&fixture.app, &["rev-parse", TASK_BRANCH]);
    assert_eq!(first.head, task_head);
    assert_eq!(current_branch(&tip), run_branch_name(TASK_BRANCH, RUN_1, 1));

    // The job builds, edits a tracked file and leaves an untracked one; the run succeeds.
    std::fs::create_dir_all(tip.join("target")).unwrap();
    std::fs::write(tip.join("target/app.bin"), "build output").unwrap();
    std::fs::write(tip.join("README.md"), "changed\n").unwrap();
    std::fs::write(tip.join("scratch.txt"), "left behind\n").unwrap();

    for (run, previous) in [(RUN_2, RUN_1), (RUN_3, RUN_2)] {
        let job = fixture.task_job(run, Some(last(previous, LastEnd::Succeeded)));
        let outcome = fixture.reset(&job).await.unwrap();
        assert!(!outcome.added);
        assert_eq!(outcome.saved, None, "a succeeded run's tree is not saved");
        assert_eq!(current_branch(&tip), job.run_branch);
        assert_eq!(
            std::fs::read_to_string(tip.join("README.md")).unwrap(),
            "readme\n"
        );
        assert!(
            !tip.join("scratch.txt").exists(),
            "untracked files are removed"
        );
        assert_eq!(
            std::fs::read_to_string(tip.join("target/app.bin")).unwrap(),
            "build output",
            "ignored build output stays"
        );
        assert_eq!(git(&tip, &["status", "--porcelain"]), "");
    }

    assert_eq!(fixture.untouched(), before);
    assert_eq!(git(&fixture.app, &["status", "--porcelain"]), main_checkout);
    assert_eq!(current_branch(&fixture.app), "main");
    assert_eq!(git(&fixture.app, &["rev-parse", TASK_BRANCH]), task_head);
}

#[gpui::test]
async fn a_failed_runs_work_is_saved_on_its_run_branch_with_large_files_listed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    // The profile puts the named directory `target` inside the tree; it is neither saved nor
    // cleaned.
    let fixture = Fixture::with_profile(|base| {
        let keep = base.join("data/worktrees/app/lane-build-1/keep");
        Some(FakeProfile::default().at("build-1", "target", keep))
    });
    let tip = fixture.tip();
    let keep = tip.join("keep");
    fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    let run_branch = run_branch_name(TASK_BRANCH, RUN_1, 1);
    let start = git(&tip, &["rev-parse", "HEAD"]);

    std::fs::write(tip.join("README.md"), "half done\n").unwrap();
    std::fs::write(tip.join("new.txt"), "new work\n").unwrap();
    std::fs::write(tip.join("big.bin"), vec![b'x'; 2000]).unwrap();
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join("cache"), "named directory").unwrap();
    std::fs::create_dir_all(tip.join("target")).unwrap();
    std::fs::write(tip.join("target/out"), "ignored").unwrap();

    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Failed)));
    let outcome = fixture.reset(&job).await.unwrap();
    let saved = outcome.saved.unwrap();
    let Some((commit, SaveTarget::Branch(branch))) = saved.commit.clone() else {
        panic!("expected a save on the run branch: {saved:?}");
    };
    assert_eq!(branch, run_branch);
    assert_eq!(git(&fixture.app, &["rev-parse", &run_branch]), commit);
    assert_eq!(
        git(&fixture.app, &["rev-parse", &format!("{commit}^")]),
        start
    );
    assert_eq!(
        git(
            &fixture.app,
            &["log", "-1", "--format=%s|%an <%ae>|%cn <%ce>", &commit]
        ),
        format!(
            "work left by failed run {RUN_1}|bongos <bongos@localhost>|bongos <bongos@localhost>"
        )
    );
    let files = git(&fixture.app, &["ls-tree", "-r", "--name-only", &commit]);
    let files: Vec<&str> = files.lines().collect();
    assert_eq!(files, [".gitignore", "README.md", "new.txt"]);
    assert_eq!(
        git(&fixture.app, &["show", &format!("{commit}:README.md")]),
        "half done"
    );
    assert_eq!(saved.skipped, [("big.bin".to_string(), 2000)]);
    assert!(
        saved
            .history()
            .contains("too large to save: big.bin (2000 bytes)")
    );

    // Then the reset: the new run branch, the large file gone, the named directory and the
    // ignored output kept. The task branch did not move.
    assert_eq!(current_branch(&tip), job.run_branch);
    assert!(!tip.join("big.bin").exists() && !tip.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(keep.join("cache")).unwrap(),
        "named directory"
    );
    assert!(tip.join("target/out").exists());
    assert_eq!(
        git(&fixture.app, &["rev-parse", TASK_BRANCH]),
        git(&fixture.app, &["rev-parse", "main"])
    );
}

#[gpui::test]
async fn work_off_the_run_branch_is_saved_at_a_saved_ref_never_on_a_task_branch(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    let task_y = git(&fixture.app, &["rev-parse", "task/y"]);
    let run_1 = git(
        &fixture.app,
        &["rev-parse", &run_branch_name(TASK_BRANCH, RUN_1, 1)],
    );

    // Detached, after a halted run.
    git(&tip, &["checkout", "-q", "--detach"]);
    std::fs::write(tip.join("README.md"), "detached work\n").unwrap();
    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Halted)));
    let saved = fixture.reset(&job).await.unwrap().saved.unwrap();
    let name = format!("refs/bongos/saved/build-1/{RUN_1}");
    let Some((commit, SaveTarget::Ref(target))) = saved.commit else {
        panic!("expected a saved ref");
    };
    assert_eq!(target, name);
    assert_eq!(git(&fixture.app, &["rev-parse", &name]), commit);
    assert_eq!(
        git(&fixture.app, &["log", "-1", "--format=%s", &commit]),
        format!("work left by halted run {RUN_1}")
    );

    // On a task branch (the tree after a merge), after a cancelled run.
    git(&tip, &["checkout", "-q", "task/y"]);
    std::fs::write(tip.join("other.txt"), "on the task branch\n").unwrap();
    let job = fixture.task_job(RUN_3, Some(last(RUN_2, LastEnd::Cancelled)));
    let saved = fixture.reset(&job).await.unwrap().saved.unwrap();
    let Some((_, SaveTarget::Ref(target))) = saved.commit else {
        panic!("expected a saved ref");
    };
    assert_eq!(target, format!("refs/bongos/saved/build-1/{RUN_2}"));
    assert_eq!(git(&fixture.app, &["rev-parse", "task/y"]), task_y);
    assert_eq!(
        git(
            &fixture.app,
            &["rev-parse", &run_branch_name(TASK_BRANCH, RUN_1, 1)]
        ),
        run_1
    );
    assert_eq!(current_branch(&tip), job.run_branch);
}

#[gpui::test]
async fn a_second_tree_is_never_saved(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let second = fixture.locations.get("tree:app:base").unwrap().path.clone();
    let first = fixture.task_job(RUN_1, None);
    reset_tree(&fixture.git, &fixture.request(&first, true))
        .await
        .unwrap();
    std::fs::write(second.join("README.md"), "edited in the second tree\n").unwrap();
    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Failed)));
    let outcome = reset_tree(&fixture.git, &fixture.request(&job, true))
        .await
        .unwrap();
    assert_eq!(outcome.saved, None);
    assert_eq!(
        git(&fixture.app, &["for-each-ref", "refs/bongos/"]),
        "",
        "no saved ref"
    );
    assert_eq!(
        std::fs::read_to_string(second.join("README.md")).unwrap(),
        "readme\n"
    );
}

#[gpui::test]
async fn a_failed_save_or_reset_deletes_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    std::fs::write(tip.join("README.md"), "precious\n").unwrap();
    std::fs::write(tip.join("new.txt"), "also precious\n").unwrap();
    let run_1 = git(
        &fixture.app,
        &["rev-parse", &run_branch_name(TASK_BRANCH, RUN_1, 1)],
    );

    // The save cannot make its index: nothing is reset.
    let index = PathBuf::from(git(
        &tip,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "bongos-save-index",
        ],
    ));
    std::fs::create_dir_all(index.join("blocker")).unwrap();
    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Failed)));
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(error.contains("saving the work of run"), "{error}");
    assert!(error.ends_with("nothing was reset"), "{error}");
    let still = |tip: &Path| {
        assert_eq!(
            std::fs::read_to_string(tip.join("README.md")).unwrap(),
            "precious\n"
        );
        assert_eq!(
            std::fs::read_to_string(tip.join("new.txt")).unwrap(),
            "also precious\n"
        );
        assert_eq!(current_branch(tip), run_branch_name(TASK_BRANCH, RUN_1, 1));
    };
    still(&tip);
    assert_eq!(
        git(
            &fixture.app,
            &["rev-parse", &run_branch_name(TASK_BRANCH, RUN_1, 1)]
        ),
        run_1
    );
    std::fs::remove_dir_all(&index).unwrap();

    // A git command of the reset fails (the index is locked): an error, and nothing deleted.
    let lock = PathBuf::from(git(
        &tip,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ],
    ));
    std::fs::write(&lock, "").unwrap();
    let job = fixture.task_job(RUN_2, None);
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(error.contains("reset"), "{error}");
    still(&tip);
    std::fs::remove_file(&lock).unwrap();
}

#[gpui::test]
async fn a_crash_after_the_reset_starts_again_and_a_run_branch_with_work_is_never_moved(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let job = fixture.task_job(RUN_1, None);
    let first = fixture.reset(&job).await.unwrap();
    // The app stopped before the run started: the same start again finds the tree on the new run
    // branch and resets it in place.
    let again = fixture.reset(&job).await.unwrap();
    assert_eq!(again.head, first.head);
    assert_eq!(current_branch(&tip), job.run_branch);

    // A run branch that holds commits is refused, not reset.
    std::fs::write(tip.join("work.txt"), "authored\n").unwrap();
    git(&tip, &["add", "work.txt"]);
    git(&tip, &["commit", "-q", "-m", "authored"]);
    let authored = git(&tip, &["rev-parse", "HEAD"]);
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(git(&fixture.app, &["rev-parse", &job.run_branch]), authored);
    assert!(tip.join("work.txt").exists());
}

#[gpui::test]
async fn a_lane_never_resets_a_path_that_is_not_its_own_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let forbidden = fixture.forbidden();
    let before = fixture.untouched();
    let app_before = snapshot(&fixture.app);
    let other_lane = fixture.all[1].get("tree:app").unwrap().path.clone();

    // As written: only the lane's own resolved tree.
    let error = check_own_tree(&other_lane, &tip, &forbidden).unwrap_err();
    assert!(error.contains("the lane's tree is"), "{error}");

    // A location that resolves to a forbidden place is refused before git runs.
    let job = fixture.task_job(RUN_1, None);
    for (path, why) in [
        (fixture.app.clone(), "it is the primary checkout"),
        (fixture.checkout.clone(), "it is the primary checkout"),
        (fixture.task_worktree.clone(), "it is the task worktree"),
        (other_lane.clone(), "another lane's location"),
        (fixture.app.join("inner"), "inside the primary checkout"),
    ] {
        let mut request = fixture.request(&job, false);
        let item = request
            .locations
            .items
            .iter_mut()
            .find(|item| item.name == "tree:app")
            .unwrap();
        item.path = path.clone();
        let error = reset_tree(&fixture.git, &request).await.unwrap_err();
        assert!(error.contains(why), "{}: {error}", path.display());
    }

    // A link at the tree's place, to the user's checkout or through a linked parent, is refused.
    std::fs::create_dir_all(tip.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&fixture.checkout, &tip).unwrap();
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    std::fs::remove_file(&tip).unwrap();
    let parent = tip.parent().unwrap().to_path_buf();
    std::fs::remove_dir(&parent).unwrap();
    std::os::unix::fs::symlink(&fixture.task_worktree, &parent).unwrap();
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(error.contains("inside the task worktree"), "{error}");
    std::fs::remove_file(&parent).unwrap();

    // A plain directory at the tree's place, inside some other repository, is not a worktree of
    // the repository: git is asked and nothing is reset.
    let data = fixture.base.join("data");
    git(&data, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(&tip).unwrap();
    std::fs::write(tip.join("file.txt"), "not a lane tree\n").unwrap();
    let error = fixture.reset(&job).await.unwrap_err();
    assert!(
        error.contains("is not a git worktree of its own"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(tip.join("file.txt")).unwrap(),
        "not a lane tree\n"
    );

    assert_eq!(fixture.untouched(), before);
    assert_eq!(snapshot(&fixture.app), app_before);
}

fn lane_merge(fixture: &Fixture, target: MergeTarget, run_branch: &str, run_id: &str) -> LaneMerge {
    LaneMerge {
        locations: fixture.locations.clone(),
        repo: "app".to_string(),
        repository: fixture.app.clone(),
        forbidden: fixture.forbidden(),
        target,
        run_branch: run_branch.to_string(),
        run_id: run_id.to_string(),
        workflow: "build".to_string(),
        todo: "ship it".to_string(),
        style: MergeStyle::FfOrMerge,
    }
}

fn commit_in(tree: &Path, file: &str) -> String {
    std::fs::write(tree.join(file), format!("{file}\n")).unwrap();
    git(tree, &["add", file]);
    git(tree, &["commit", "-q", "-m", file]);
    git(tree, &["rev-parse", "HEAD"])
}

#[gpui::test]
async fn a_project_job_starts_from_its_start_and_merges_into_merge_into(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let job = JobRepository::for_project(
        "app",
        fixture.app.clone(),
        Some("release"),
        "main",
        RUN_1,
        1,
        None,
    );
    assert_eq!(job.run_branch, run_branch_name("release", RUN_1, 1));
    let outcome = fixture.reset(&job).await.unwrap();
    assert_eq!(outcome.head, git(&fixture.app, &["rev-parse", "release"]));
    assert_eq!(current_branch(&tip), job.run_branch);
    assert!(tip.join("RELEASE.md").exists());
    let work = commit_in(&tip, "feature.txt");

    // No merge_into: nothing is merged.
    let integration = git(&fixture.app, &["rev-parse", "integration"]);
    let nothing = merge_in_lane_tree(
        &fixture.git,
        &lane_merge(
            &fixture,
            MergeTarget::MergeInto(None),
            &job.run_branch,
            RUN_1,
        ),
    )
    .await
    .unwrap();
    assert_eq!(nothing, LaneMerged::Nothing);
    assert_eq!(
        git(&fixture.app, &["rev-parse", "integration"]),
        integration
    );
    assert_eq!(current_branch(&tip), job.run_branch);

    // integration checked out in another worktree (the user's): refused, the run branch intact.
    let elsewhere = fixture.base.join("elsewhere");
    git(
        &fixture.app,
        &[
            "worktree",
            "add",
            "-q",
            &elsewhere.to_string_lossy(),
            "integration",
        ],
    );
    let into = MergeTarget::MergeInto(Some("integration".to_string()));
    let refused = merge_in_lane_tree(
        &fixture.git,
        &lane_merge(&fixture, into.clone(), &job.run_branch, RUN_1),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&refused, LaneMergeError::CheckedOutElsewhere { branch, .. } if branch == "integration"),
        "{refused:?}"
    );
    assert_eq!(git(&fixture.app, &["rev-parse", &job.run_branch]), work);
    assert_eq!(
        git(&fixture.app, &["rev-parse", "integration"]),
        integration
    );
    assert_eq!(current_branch(&tip), job.run_branch);
    git(
        &fixture.app,
        &["worktree", "remove", &elsewhere.to_string_lossy()],
    );

    // At success the run branch is merged into integration in the lane tree.
    let merged = merge_in_lane_tree(
        &fixture.git,
        &lane_merge(&fixture, into, &job.run_branch, RUN_1),
    )
    .await
    .unwrap();
    let LaneMerged::Merged(Merged::FastForward { head } | Merged::MergeCommit { head }) = merged
    else {
        panic!("expected a merge: {merged:?}");
    };
    assert_eq!(git(&fixture.app, &["rev-parse", "integration"]), head);
    git(
        &fixture.app,
        &["merge-base", "--is-ancestor", &work, "integration"],
    );
    assert_eq!(git(&fixture.app, &["rev-parse", &job.run_branch]), work);
    assert_eq!(current_branch(&fixture.app), "main");

    // The next job's reset moves the tree off integration again.
    let next = JobRepository::for_project("app", fixture.app.clone(), None, "main", RUN_2, 1, None);
    fixture.reset(&next).await.unwrap();
    assert_eq!(current_branch(&tip), next.run_branch);
    assert_eq!(git(&fixture.app, &["rev-parse", "integration"]), head);
}

#[gpui::test]
async fn a_branch_only_leaf_merges_into_its_task_branch_in_the_lane_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let before = fixture.untouched();
    let job = JobRepository::for_task("app", fixture.app.clone(), "task/y", RUN_1, 1, None);
    fixture.reset(&job).await.unwrap();
    let work = commit_in(&tip, "leaf.txt");
    let merged = merge_in_lane_tree(
        &fixture.git,
        &lane_merge(
            &fixture,
            MergeTarget::TaskBranch("task/y".to_string()),
            &job.run_branch,
            RUN_1,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        merged,
        LaneMerged::Merged(Merged::FastForward { head: work.clone() })
    );
    assert_eq!(git(&fixture.app, &["rev-parse", "task/y"]), work);
    assert_eq!(current_branch(&tip), "task/y");

    // A task branch with its own worktree (a hand-started task) is never checked out here.
    let refused = merge_in_lane_tree(
        &fixture.git,
        &lane_merge(
            &fixture,
            MergeTarget::TaskBranch(TASK_BRANCH.to_string()),
            &job.run_branch,
            RUN_1,
        ),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, LaneMergeError::CheckedOutElsewhere { .. }),
        "{refused:?}"
    );
    assert_eq!(fixture.untouched(), before);
}

#[gpui::test]
async fn first_use_adds_the_tree_detached_through_the_tree_maker(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let maker = crate::lane_tree::git_tree_maker(
        fixture.git.clone(),
        BTreeMap::from([("app".to_string(), "release".to_string())]),
    );
    std::fs::create_dir_all(tip.parent().unwrap()).unwrap();
    maker(&crate::prepare::TreeRequest {
        lane: LaneId::new("build", 1),
        repo: "app".to_string(),
        repository_path: fixture.app.clone(),
        path: tip.clone(),
        base: false,
    })
    .unwrap();
    assert_eq!(current_branch(&tip), "", "detached");
    assert_eq!(
        git(&tip, &["rev-parse", "HEAD"]),
        git(&fixture.app, &["rev-parse", "release"])
    );
    // The reset then finds it and puts it on the run branch.
    let job = fixture.task_job(RUN_1, None);
    let outcome = fixture.reset(&job).await.unwrap();
    assert!(!outcome.added);
    assert_eq!(current_branch(&tip), job.run_branch);
    assert_eq!(current_branch(&fixture.app), "main");
}

#[gpui::test]
async fn a_restart_on_the_same_run_branch_keeps_the_failed_runs_work(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let first = fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    std::fs::write(tip.join("README.md"), "half done\n").unwrap();
    std::fs::write(tip.join("new.txt"), "new work\n").unwrap();

    // The same run starts again after it failed: its run branch is the one the tree is on.
    let job = fixture.task_job(RUN_1, Some(last(RUN_1, LastEnd::Failed)));
    assert_eq!(job.run_branch, last(RUN_1, LastEnd::Failed).run_branch);
    let outcome = fixture.reset(&job).await.unwrap();
    let saved = outcome.saved.unwrap();
    let Some((commit, SaveTarget::Ref(name))) = saved.commit else {
        panic!("expected a saved ref: {saved:?}");
    };
    assert_eq!(name, format!("refs/bongos/saved/build-1/{RUN_1}"));
    assert_eq!(git(&fixture.app, &["rev-parse", &name]), commit);
    assert_eq!(
        git(&fixture.app, &["show", &format!("{commit}:README.md")]),
        "half done"
    );
    assert_eq!(
        git(&fixture.app, &["show", &format!("{commit}:new.txt")]),
        "new work"
    );
    assert_eq!(
        git(&fixture.app, &["rev-parse", &format!("{commit}^")]),
        first.head
    );
    // The run branch is back at its start, and the tree is clean on it.
    assert_eq!(git(&fixture.app, &["rev-parse", &job.run_branch]), first.head);
    assert_eq!(current_branch(&tip), job.run_branch);
    assert_eq!(git(&tip, &["status", "--porcelain"]), "");
}

#[gpui::test]
async fn staged_work_and_added_then_deleted_files_are_saved(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let start = fixture
        .reset(&fixture.task_job(RUN_1, None))
        .await
        .unwrap()
        .head;
    // Added, then deleted from the disk.
    std::fs::write(tip.join("gone.txt"), "briefly\n").unwrap();
    git(&tip, &["add", "gone.txt"]);
    std::fs::remove_file(tip.join("gone.txt")).unwrap();
    // Staged, then the file on disk put back as it was.
    std::fs::write(tip.join("README.md"), "staged\n").unwrap();
    git(&tip, &["add", "README.md"]);
    std::fs::write(tip.join("README.md"), "readme\n").unwrap();
    std::fs::write(tip.join("new.txt"), "on disk\n").unwrap();

    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Failed)));
    let saved = fixture.reset(&job).await.unwrap().saved.unwrap();
    let Some((commit, SaveTarget::Branch(branch))) = saved.commit else {
        panic!("expected a save on the run branch: {saved:?}");
    };
    assert_eq!(branch, run_branch_name(TASK_BRANCH, RUN_1, 1));
    assert_eq!(git(&fixture.app, &["rev-parse", &branch]), commit);
    let staged = git(&fixture.app, &["rev-parse", &format!("{commit}^")]);
    assert_eq!(
        git(&fixture.app, &["rev-parse", &format!("{staged}^")]),
        start
    );
    assert_eq!(
        git(&fixture.app, &["log", "-1", "--format=%s", &staged]),
        format!("staged work left by failed run {RUN_1}")
    );
    assert_eq!(
        git(&fixture.app, &["show", &format!("{staged}:README.md")]),
        "staged"
    );
    assert_eq!(
        git(&fixture.app, &["show", &format!("{staged}:gone.txt")]),
        "briefly"
    );
    assert_eq!(
        git(&fixture.app, &["ls-tree", "-r", "--name-only", &commit]),
        ".gitignore\nREADME.md\nnew.txt"
    );
    assert_eq!(
        git(&fixture.app, &["show", &format!("{commit}:README.md")]),
        "readme"
    );
    assert_eq!(current_branch(&tip), job.run_branch);
    assert_eq!(git(&tip, &["status", "--porcelain"]), "");
}

#[gpui::test]
async fn a_file_name_that_is_not_utf8_is_saved(cx: &mut TestAppContext) {
    use std::os::unix::ffi::OsStrExt;
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    if std::fs::write(tip.join(name), "bytes\n").is_err() {
        // The filesystem refuses the name; nothing to check here.
        return;
    }
    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Failed)));
    let saved = fixture.reset(&job).await.unwrap().saved.unwrap();
    let Some((commit, _)) = saved.commit else {
        panic!("expected a save: {saved:?}");
    };
    // `core.quotePath` prints the byte as octal.
    let files = git(&fixture.app, &["ls-tree", "-r", "--name-only", &commit]);
    assert!(files.contains("\"caf\\351.txt\""), "{files}");
    assert!(!tip.join(name).exists());
}

#[gpui::test]
async fn hooks_and_signing_config_never_stop_a_save(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    fixture.reset(&fixture.task_job(RUN_1, None)).await.unwrap();
    // A hook that rejects every saved ref, and signing that cannot work.
    let hooks = fixture.base.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    std::fs::write(
        &hook,
        "#!/bin/sh\nif grep -q refs/bongos/; then exit 1; fi\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &fixture.app,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    git(&fixture.app, &["config", "commit.gpgSign", "true"]);
    git(&fixture.app, &["config", "gpg.program", "false"]);

    git(&tip, &["checkout", "-q", "--detach"]);
    std::fs::write(tip.join("README.md"), "detached work\n").unwrap();
    let job = fixture.task_job(RUN_2, Some(last(RUN_1, LastEnd::Halted)));
    let saved = fixture.reset(&job).await.unwrap().saved.unwrap();
    let Some((commit, SaveTarget::Ref(name))) = saved.commit else {
        panic!("expected a saved ref: {saved:?}");
    };
    assert_eq!(git(&fixture.app, &["rev-parse", &name]), commit);
    assert_eq!(
        git(&fixture.app, &["show", &format!("{commit}:README.md")]),
        "detached work"
    );
}
