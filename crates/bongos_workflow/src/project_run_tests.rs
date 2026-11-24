//! Tests of runs with no task and of lane runs: a project run on a repository works in a run
//! tree made from its base on its own run branch and merges into its target only at its success
//! end, and when another worktree has the target checked out it waits on a person with the run
//! branch and that checkout unchanged; a run on a snapshot works in the snapshot with no tree or
//! branch of its own; an agent of a project run is told about the project, never a task; a lane
//! run carries its lane, directories and environment edits, and a start with none of them is
//! set up as a task run always was. A project run with no repository works in its own work
//! directory with no tree, branch or git, its programs may write only there, its documents are
//! copied out at free names and listed, and `clear finished` removes that directory and nothing
//! else. Real git, isolated from the real configuration, only where
//! the test is about git; the runs are the scripted ones of the lifecycle tests, a child of which
//! this is.

use futures::executor::block_on;

use super::*;
use crate::builtin_merge::ProjectMergeRequest;
use crate::lifecycle::{
    PROJECT_WORK_DIR, ProjectEnd, project_settlement, project_work_dir, remove_project_work_dir,
};
use crate::runtime::RunSetup;
use crate::task_link::{MERGE_GATE, gate_key};
use crate::worktrees::run_branch_name;
use bongos_workflow_model::{EnvEdit, FailureReason, RunState, RunTreeRecord};

/// The project's folder in the world: its top group's directory.
fn project_dir(world: &World) -> PathBuf {
    world.tree.projects[0].task.dir.clone()
}

fn git_out(git: &Git, dir: &Path, args: &[&str]) -> String {
    block_on(git.run(dir, args)).unwrap()
}

/// A lifecycle over real git that holds a run at its succeeded end for the settlement.
fn git_lifecycle(world: &mut World) -> (Lifecycle<FakeRunner>, Git, PathBuf) {
    let worktrees = real_git(world);
    let git = worktrees.git.clone();
    let mut lifecycle = Lifecycle::new(FakeRunner::new(), Box::new(worktrees), world.settings());
    lifecycle.runtime_mut().hold_success(true);
    (lifecycle, git, world.root.join("repo"))
}

fn repository_request(repo: &Path, merge_into: &str) -> StartRequest {
    StartRequest {
        tree: RunTree::Fresh {
            repo: Some(repo.to_path_buf()),
            base: Some("main".to_string()),
            merge_into: Some(merge_into.to_string()),
        },
        ..project_request("single")
    }
}

/// Commits `file` in the run tree, as a node would.
fn commit_in(git: &Git, tree: &Path, file: &str) -> String {
    fs::write(tree.join(file), "work\n").unwrap();
    git_out(git, tree, &["add", file]);
    git_out(git, tree, &["commit", "-q", "-m", file]);
    git_out(git, tree, &["rev-parse", "HEAD"])
}

/// Runs every node to the succeeded end and settles what the end of a run with no task does.
fn end_project_run(lifecycle: &mut Lifecycle<FakeRunner>, git: &Git, run: &str) {
    finish_all(lifecycle);
    assert_eq!(lifecycle.take_settling(), vec![run.to_string()]);
    settle_once(lifecycle, git, run);
}

fn settle_once(lifecycle: &mut Lifecycle<FakeRunner>, git: &Git, run: &str) {
    let end = lifecycle.project_end(run).unwrap();
    let settled = block_on(project_settlement(git, &end));
    lifecycle.settle_project(run, settled).unwrap();
}

fn is_ancestor(git: &Git, dir: &Path, ancestor: &str, of: &str) -> bool {
    block_on(git.output(dir, &["merge-base", "--is-ancestor", ancestor, of]))
        .unwrap()
        .status
        .success()
}

#[test]
fn a_project_run_on_a_repository_works_on_a_run_branch_from_its_base_and_merges_at_its_end() {
    let mut world = World::new();
    let (mut lifecycle, git, repo) = git_lifecycle(&mut world);
    git_out(&git, &repo, &["branch", "integration"]);
    let main_head = git_out(&git, &repo, &["rev-parse", "main"]);

    let run = world
        .start(&mut lifecycle, repository_request(&repo, "integration"))
        .unwrap();
    let tree = lifecycle.worktree(&run).unwrap().clone();
    assert_eq!(tree.branch, run_branch_name("main", &run, 1));
    assert_eq!(
        git_out(&git, &tree.path, &["symbolic-ref", "--short", "HEAD"]),
        tree.branch
    );
    assert_eq!(git_out(&git, &tree.path, &["rev-parse", "HEAD"]), main_head);
    assert_ne!(
        tree.path, repo,
        "the run never works in a person's checkout"
    );
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(
        record.subject,
        RunSubject::Project {
            id: GROUP.to_string()
        }
    );
    assert_eq!(record.task_id, "");
    assert_eq!(
        record.tree,
        Some(RunTreeRecord::Repository {
            repo: repo.clone(),
            base: "main".to_string(),
            merge_into: Some("integration".to_string()),
        })
    );
    assert_eq!(
        lifecycle.run_dir(&run).unwrap(),
        project_dir(&world).join("runs").join(&run)
    );

    let work = commit_in(&git, &tree.path, "feature.txt");
    // Nothing is merged before the success end.
    assert_eq!(
        git_out(&git, &repo, &["rev-parse", "integration"]),
        main_head
    );
    end_project_run(&mut lifecycle, &git, &run);

    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert!(is_ancestor(&git, &repo, &work, "integration"));
    assert_eq!(git_out(&git, &repo, &["rev-parse", &tree.branch]), work);
    // The run tree went back to its run branch, so it never holds the target.
    assert_eq!(
        git_out(&git, &tree.path, &["symbolic-ref", "--short", "HEAD"]),
        tree.branch
    );
    // The person's checkout is untouched.
    assert_eq!(git_out(&git, &repo, &["rev-parse", "main"]), main_head);
    assert_eq!(
        git_out(
            &git,
            &repo,
            &["status", "--porcelain", "--untracked-files=no"]
        ),
        ""
    );
    assert!(!repo.join("feature.txt").exists());
}

#[test]
fn a_project_run_whose_target_is_checked_out_elsewhere_waits_with_its_branch_and_that_checkout_unchanged()
 {
    let mut world = World::new();
    let (mut lifecycle, git, repo) = git_lifecycle(&mut world);
    let main_head = git_out(&git, &repo, &["rev-parse", "main"]);
    let run = world
        .start(&mut lifecycle, repository_request(&repo, "main"))
        .unwrap();
    let tree = lifecycle.worktree(&run).unwrap().clone();
    let work = commit_in(&git, &tree.path, "feature.txt");
    end_project_run(&mut lifecycle, &git, &run);

    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    let key = gate_key(MERGE_GATE);
    assert_eq!(lifecycle.open_gates(&run), vec![key.clone()]);
    let state = lifecycle.state(&run).unwrap();
    assert_eq!(
        state.open_gates[&key],
        format!(
            "branch main is checked out at {}; merge it there",
            repo.display()
        )
    );
    let unchanged = |lifecycle: &Lifecycle<FakeRunner>| {
        assert_eq!(git_out(&git, &repo, &["rev-parse", "main"]), main_head);
        assert_eq!(
            git_out(
                &git,
                &repo,
                &["status", "--porcelain", "--untracked-files=no"]
            ),
            ""
        );
        assert!(!repo.join("feature.txt").exists());
        assert_eq!(git_out(&git, &repo, &["rev-parse", &tree.branch]), work);
        assert_eq!(
            git_out(&git, &tree.path, &["symbolic-ref", "--short", "HEAD"]),
            tree.branch
        );
        assert!(lifecycle.project_end(&run).is_some_and(|end| !end.merged));
    };
    unchanged(&lifecycle);

    // Approved while the checkout still holds it: the run waits again, nothing changed.
    lifecycle.approve(&run, &key, None).unwrap();
    settle(&mut lifecycle);
    assert_eq!(lifecycle.take_settling(), vec![run.clone()]);
    settle_once(&mut lifecycle, &git, &run);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    unchanged(&lifecycle);

    // The person merges it there and approves: the run ends with nothing more merged.
    git_out(&git, &repo, &["merge", "-q", "--ff-only", &tree.branch]);
    lifecycle.approve(&run, &key, None).unwrap();
    settle(&mut lifecycle);
    assert_eq!(lifecycle.take_settling(), vec![run.clone()]);
    settle_once(&mut lifecycle, &git, &run);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(git_out(&git, &repo, &["rev-parse", "main"]), work);
}

#[test]
fn a_project_merge_never_checks_out_its_target_so_a_retry_after_a_crash_finds_the_run_tree_on_its_branch()
 {
    use crate::builtin_merge::{ProjectMergeRefusal, ProjectMerged, merge_project_run};
    let mut world = World::new();
    let (_lifecycle, git, repo) = git_lifecycle(&mut world);
    let path = |dir: &Path| dir.to_string_lossy().into_owned();
    let tree = world.root.join("run-tree");
    git_out(
        &git,
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "run-branch",
            &path(&tree),
            "main",
        ],
    );
    let work = commit_in(&git, &tree, "feature.txt");
    // `integration` moved on, held by no worktree, so the merge needs a merge commit.
    let moved = git_out(
        &git,
        &repo,
        &["commit-tree", "main^{tree}", "-p", "main", "-m", "moved"],
    );
    git_out(&git, &repo, &["branch", "integration", &moved]);
    // `clash` changes the same file, held by no worktree.
    let clash_tree = world.root.join("clash-tree");
    git_out(
        &git,
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "clash",
            &path(&clash_tree),
            "main",
        ],
    );
    fs::write(clash_tree.join("feature.txt"), "other\n").unwrap();
    git_out(&git, &clash_tree, &["add", "feature.txt"]);
    git_out(&git, &clash_tree, &["commit", "-q", "-m", "clash"]);
    git_out(&git, &clash_tree, &["checkout", "-q", "--detach"]);
    let clash_head = git_out(&git, &repo, &["rev-parse", "clash"]);
    // The run tree's git directory, as the run records it when it makes the tree.
    let run_git_dir = PathBuf::from(git_out(
        &git,
        &tree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ));
    let request = |merge_into: &str| ProjectMergeRequest {
        run_tree: tree.clone(),
        run_git_dir: run_git_dir.clone(),
        run_branch: "run-branch".to_string(),
        merge_into: merge_into.to_string(),
        run_id: "run-1".to_string(),
        workflow: "single".to_string(),
    };
    let on_run_branch = || {
        assert_eq!(
            git_out(&git, &tree, &["symbolic-ref", "--short", "HEAD"]),
            "run-branch"
        );
        assert_eq!(git_out(&git, &tree, &["status", "--porcelain"]), "");
        assert_eq!(git_out(&git, &tree, &["rev-parse", "HEAD"]), work);
    };

    // A conflict writes nothing.
    assert_eq!(
        block_on(merge_project_run(&git, &request("clash"))),
        Err(ProjectMergeRefusal::Conflict(vec![
            "feature.txt".to_string()
        ]))
    );
    assert_eq!(git_out(&git, &repo, &["rev-parse", "clash"]), clash_head);
    on_run_branch();

    // The merge commit lands on `integration` while the run tree stays on its branch.
    let Ok(ProjectMerged::Merged { head }) =
        block_on(merge_project_run(&git, &request("integration")))
    else {
        panic!("not merged");
    };
    assert_eq!(git_out(&git, &repo, &["rev-parse", "integration"]), head);
    assert_eq!(
        git_out(
            &git,
            &repo,
            &["rev-parse", &format!("{head}^1"), &format!("{head}^2")]
        ),
        format!("{moved}\n{work}")
    );
    assert_eq!(
        git_out(&git, &repo, &["show", &format!("{head}:feature.txt")]),
        "work"
    );
    on_run_branch();

    // A retry, as after a crash once the merge landed, finds it done and the tree as it was.
    assert_eq!(
        block_on(merge_project_run(&git, &request("integration"))),
        Ok(ProjectMerged::AlreadyIn)
    );
    on_run_branch();

    // So a person can check the target out, and a later merge into it then waits on them.
    git_out(&git, &repo, &["checkout", "-q", "integration"]);
    let more = commit_in(&git, &tree, "more.txt");
    assert_eq!(
        block_on(merge_project_run(&git, &request("integration"))),
        Err(ProjectMergeRefusal::CheckedOut {
            branch: "integration".to_string(),
            path: repo.clone(),
        })
    );
    assert_eq!(git_out(&git, &repo, &["rev-parse", "integration"]), head);
    assert_eq!(
        git_out(
            &git,
            &repo,
            &["status", "--porcelain", "--untracked-files=no"]
        ),
        ""
    );
    assert_eq!(git_out(&git, &tree, &["rev-parse", "HEAD"]), more);
}

#[test]
fn a_project_run_with_no_target_merges_nothing() {
    let end = ProjectEnd {
        merge: None,
        merged: false,
        documents: None,
    };
    let settled = block_on(project_settlement(&Git::new("git"), &end));
    assert_eq!(settled.settlement, crate::runtime::Settlement::Succeed);
    assert_eq!(settled.merged_head, None);
    // A merge already recorded is never made again.
    let end = ProjectEnd {
        merge: Some(ProjectMergeRequest {
            run_tree: PathBuf::from("/nowhere"),
            run_git_dir: PathBuf::from("/nowhere/.git"),
            run_branch: "run".to_string(),
            merge_into: "main".to_string(),
            run_id: "run".to_string(),
            workflow: "single".to_string(),
        }),
        merged: true,
        documents: None,
    };
    let settled = block_on(project_settlement(&Git::new("git"), &end));
    assert_eq!(settled.settlement, crate::runtime::Settlement::Succeed);
}

/// A snapshot directory holding a project document.
fn snapshot(world: &World) -> PathBuf {
    let path = world.root.join("snapshot");
    fs::create_dir_all(path.join("documents")).unwrap();
    fs::write(path.join("documents").join("roadmap.md"), "# roadmap\n").unwrap();
    path
}

#[test]
fn a_snapshot_run_works_in_the_snapshot_with_no_tree_or_branch_of_its_own() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let path = snapshot(&world);
    let request = StartRequest {
        tree: RunTree::Snapshot { path: path.clone() },
        ..project_request("single")
    };
    let run = world.start(&mut lifecycle, request).unwrap();
    let binding = lifecycle.binding(&run).unwrap();
    assert_eq!(binding.worktree.path, path);
    assert_eq!(binding.worktree.branch, "");
    assert_eq!(
        binding.worktree.run_dir,
        project_work_dir(&project_dir(&world), &run)
    );
    assert!(binding.setup.snapshot);
    assert_eq!(
        lifecycle.halt_worktrees(&run),
        None,
        "a snapshot is never offered for editing"
    );
    assert!(
        !binding.worktree.run_dir.starts_with(&path),
        "the scratch directories are outside the snapshot"
    );
    assert!(
        !world.root.join("runs").exists(),
        "no run worktree was made"
    );
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(
        record.tree,
        Some(RunTreeRecord::Snapshot { path: path.clone() })
    );
    assert_eq!(record.definitions.as_ref(), Some(&path));

    // A relaunch finds it as it was: still a snapshot, still read-only to its programs.
    let mut relaunched = world.lifecycle();
    relaunched.load_runs(&world.tree).unwrap();
    assert_eq!(relaunched.binding(&run).unwrap(), binding);

    // A missing snapshot and a task run on a snapshot are refused, and nothing starts.
    let mut fresh = world.lifecycle();
    let refusal = world
        .start(
            &mut fresh,
            StartRequest {
                tree: RunTree::Snapshot {
                    path: world.root.join("missing"),
                },
                ..project_request("single")
            },
        )
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
    assert!(fresh.all_runs().is_empty());
}

#[test]
fn a_snapshot_that_overlaps_a_directory_the_run_may_write_is_refused() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let inside_project = project_dir(&world).join("documents");
    fs::create_dir_all(&inside_project).unwrap();
    let path = snapshot(&world);
    let lane_dir = path.join("target");
    let starts = [
        // Inside the project folder, which holds the run's scratch directory.
        StartRequest {
            tree: RunTree::Snapshot {
                path: inside_project,
            },
            ..project_request("single")
        },
        // The project folder itself lies inside it.
        StartRequest {
            tree: RunTree::Snapshot {
                path: world.root.clone(),
            },
            ..project_request("single")
        },
        // It holds a lane directory.
        StartRequest {
            tree: RunTree::Snapshot { path: path.clone() },
            lane: Some(LaneRef {
                id: "lane-1".to_string(),
                kind: "build".to_string(),
            }),
            lane_dirs: [("target".to_string(), lane_dir)].into(),
            ..project_request("single")
        },
    ];
    for request in starts {
        let refusal = world.start(&mut lifecycle, request).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
        assert!(refusal.message.contains("overlaps"), "{refusal}");
    }
    assert!(lifecycle.all_runs().is_empty());
    assert!(!path.join("target").exists());
}

#[test]
fn an_agent_of_a_snapshot_run_is_pointed_at_the_snapshot_and_told_of_no_task() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let path = snapshot(&world);
    let request = StartRequest {
        tree: RunTree::Snapshot { path: path.clone() },
        ..project_request("single")
    };
    let run = world.start(&mut lifecycle, request).unwrap();
    let binding = lifecycle.binding(&run).unwrap();
    let source = binding.setup.project.as_ref().unwrap();
    assert_eq!(source.definitions, path);
    assert_eq!(source.files_dir, project_dir(&world));

    let text = crate::dispatch_agent::system_for(&binding).unwrap();
    assert!(text.starts_with("# Project context\n"), "{text}");
    assert!(text.contains("Goal: App\n"), "{text}");
    assert!(
        text.contains(&format!("Definitions: {}\n", path.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "Project documents: {}\n",
            path.join("documents").display()
        )),
        "{text}"
    );
    assert_no_task(&world, &text);
}

#[test]
fn an_agent_of_a_project_run_on_a_repository_gets_the_project_context() {
    let mut world = World::new();
    let (mut lifecycle, _git, repo) = git_lifecycle(&mut world);
    let run = world
        .start(&mut lifecycle, repository_request(&repo, "main"))
        .unwrap();
    let binding = lifecycle.binding(&run).unwrap();
    assert!(!binding.setup.snapshot);
    let text = crate::dispatch_agent::system_for(&binding).unwrap();
    let definitions = project_dir(&world);
    assert!(text.contains("Project: app\n"), "{text}");
    assert!(text.contains("Goal: App\n"), "{text}");
    assert!(
        text.contains(&format!(
            "Project documents: {}\n",
            definitions.join("documents").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("Project files: {}\n", definitions.display())),
        "{text}"
    );
    assert_no_task(&world, &text);
}

/// No task's file, directory or worktree is named in `text`.
fn assert_no_task(world: &World, text: &str) {
    let leaf = &world.tree.projects[0].task.children[0];
    let worktree = leaf.file.as_ref().unwrap().worktree.clone().unwrap();
    let needles = [
        "Task file".to_string(),
        "Task directory".to_string(),
        "Task worktree".to_string(),
        "Todo:".to_string(),
        leaf.dir.to_string_lossy().into_owned(),
        worktree,
    ];
    for needle in needles {
        assert!(!text.contains(&needle), "{needle} is in {text}");
    }
}

#[test]
fn a_lane_run_records_its_lane_directories_and_edits_and_a_plain_start_none() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let target = world.root.join("lanes").join("1").join("target");
    let request = StartRequest {
        lane: Some(LaneRef {
            id: "lane-1".to_string(),
            kind: "build".to_string(),
        }),
        lane_dirs: [("target".to_string(), target.clone())].into(),
        env: vec![EnvEdit::Set {
            name: "CARGO_TARGET_DIR".to_string(),
            value: target.to_string_lossy().into_owned(),
        }],
        started_by: StartedBy::Lane,
        ..with_workflow("single")
    };
    let run = world.start(&mut lifecycle, request.clone()).unwrap();
    let setup = lifecycle.binding(&run).unwrap().setup;
    assert_eq!(setup.lane, request.lane);
    assert_eq!(setup.lane_dirs, request.lane_dirs);
    assert_eq!(setup.env, request.env);
    assert_eq!(setup.writable_dirs(), vec![target.clone()]);
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(record.env, request.env);
    assert_eq!(record.lane_dirs, request.lane_dirs);
    assert_eq!(record.tree, None);

    // A relaunch gives the run the same setup.
    let mut relaunched = world.lifecycle();
    relaunched.load_runs(&world.tree).unwrap();
    assert_eq!(relaunched.binding(&run).unwrap().setup, setup);
    lifecycle.cancel(&run).unwrap();

    // A start with none of the new fields is set up as a task run always was.
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    assert_eq!(lifecycle.binding(&run).unwrap().setup, RunSetup::default());
    assert!(RunSetup::default().env_edits().is_empty());
    let text = fs::read_to_string(lifecycle.run_dir(&run).unwrap().join("run.toml")).unwrap();
    for field in ["tree", "definitions", "env", "lane_dirs"] {
        assert!(
            !text.lines().any(|line| line.starts_with(field)),
            "{field} in {text}"
        );
    }
    lifecycle.cancel(&run).unwrap();

    // Directories with no lane are refused.
    let refusal = world
        .start(
            &mut lifecycle,
            StartRequest {
                lane_dirs: [("target".to_string(), target)].into(),
                ..with_workflow("single")
            },
        )
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
}

/// Run worktrees that count every call and make none: a run with no repository must make no
/// worktree, read no repository and run no git.
#[derive(Clone, Default)]
struct CountingWorktrees(Arc<std::sync::atomic::AtomicUsize>);

impl CountingWorktrees {
    fn calls(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl RunWorktrees for CountingWorktrees {
    fn create(
        &mut self,
        _task: &TaskCheckout,
        _run_id: &str,
        _attempt: u32,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(WorktreeError::NotStarted)
    }

    fn base_branch(&mut self, _repo: &Path) -> Option<String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

fn counting_lifecycle(world: &World) -> (Lifecycle<FakeRunner>, CountingWorktrees) {
    let worktrees = CountingWorktrees::default();
    let mut lifecycle = Lifecycle::new(
        FakeRunner::new(),
        Box::new(worktrees.clone()),
        world.settings(),
    );
    lifecycle.runtime_mut().hold_success(true);
    (lifecycle, worktrees)
}

/// A git that is not there: settling a run with no repository must never run it.
fn no_git(world: &World) -> Git {
    Git::new(world.root.join("no-git-here"))
}

/// Runs every node to the succeeded end and settles a run with no repository, with no git.
fn end_work_run(world: &World, lifecycle: &mut Lifecycle<FakeRunner>, run: &str) -> ProjectEnd {
    finish_all(lifecycle);
    assert_eq!(lifecycle.take_settling(), vec![run.to_string()]);
    let end = lifecycle.project_end(run).unwrap();
    let settled = block_on(project_settlement(&no_git(world), &end));
    lifecycle.settle_project(run, settled).unwrap();
    end
}

/// Every file and directory under `dir`, relative, sorted; links are listed, never followed.
fn listing(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_project_run_with_no_repository_works_in_its_work_directory_with_no_tree_or_branch() {
    let world = World::new();
    let (mut lifecycle, worktrees) = counting_lifecycle(&world);
    let project = project_dir(&world);
    assert!(!project.join(PROJECT_WORK_DIR).exists());

    let run = world
        .start(&mut lifecycle, project_request("single"))
        .unwrap();
    let work = project_work_dir(&project, &run);
    assert!(
        work.is_dir(),
        "the first run makes work/<run id> and its parent"
    );
    let binding = lifecycle.binding(&run).unwrap();
    assert_eq!(
        binding.worktree.path, work,
        "BONGOS_WORKTREE is the work directory"
    );
    assert_eq!(binding.worktree.run_dir, work);
    assert_eq!(binding.worktree.branch, "");
    assert!(binding.setup.work_dir && !binding.setup.snapshot);
    assert!(binding.setup.branchless());
    assert_eq!(binding.checkout.worktree, None);
    assert_eq!(binding.checkout.branch, None);
    let key = NodeKey::new("work", 1);
    assert_eq!(
        crate::dispatch_agent::node_worktree(&key, &binding).unwrap(),
        work
    );
    let record = lifecycle.record(&run).unwrap();
    assert_eq!(record.tree, Some(RunTreeRecord::Work));
    assert_eq!(record.attempts[0].branch, "");
    assert_eq!(
        lifecycle.run_dir(&run).unwrap(),
        project.join("runs").join(&run),
        "the records stay in runs/, outside the work directory"
    );
    assert_eq!(
        lifecycle.halt_worktrees(&run),
        Some(crate::lifecycle::HaltWorktrees {
            run: work.clone(),
            branches: Vec::new(),
        })
    );
    // No worktree, no repository read, no branch, no git directory anywhere.
    assert_eq!(worktrees.calls(), 0);
    assert!(
        !world.root.join("runs").exists(),
        "no run worktree was made"
    );
    assert!(
        listing(&world.root)
            .iter()
            .all(|path| path.file_name().is_none_or(|name| name != ".git"))
    );

    // A relaunch finds it as it was.
    let mut relaunched = world.lifecycle();
    relaunched.load_runs(&world.tree).unwrap();
    assert_eq!(relaunched.binding(&run).unwrap(), binding);

    // Its end merges nothing and runs no git; the work directory stays until cleared.
    fs::write(work.join("notes.txt"), "kept\n").unwrap();
    let end = end_work_run(&world, &mut lifecycle, &run);
    assert_eq!(end.merge, None);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));
    assert_eq!(
        fs::read_to_string(work.join("notes.txt")).unwrap(),
        "kept\n"
    );
    assert_eq!(worktrees.calls(), 0);
}

#[test]
fn a_task_run_with_an_all_none_fresh_tree_still_gets_its_run_worktree() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let request = StartRequest {
        tree: RunTree::Fresh {
            repo: None,
            base: None,
            merge_into: None,
        },
        ..with_workflow("single")
    };
    let run = world.start(&mut lifecycle, request).unwrap();
    let worktree = lifecycle.worktree(&run).unwrap().clone();
    assert_eq!(
        worktree.path,
        world
            .root
            .join("runs")
            .join(format!("login-{run}-1"))
            .join("run")
    );
    assert_eq!(worktree.branch, format!("{TASK_BRANCH}-run-1"));
    let binding = lifecycle.binding(&run).unwrap();
    assert!(!binding.setup.work_dir && !binding.setup.branchless());
    assert_eq!(lifecycle.record(&run).unwrap().tree, None);
    assert!(
        !project_dir(&world).join(PROJECT_WORK_DIR).exists(),
        "a task run has no project work directory"
    );
}

const PROBE: &str = r#"
schema = 1
name = "probe"
start = "check"

[nodes.check]
kind = "program"
script = "check.py"
output = { status = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

struct StillClock(std::time::Instant);

impl crate::runner_program::Clock for StillClock {
    fn now(&self) -> std::time::Instant {
        self.0
    }
}

#[test]
fn a_program_of_a_work_directory_run_may_write_only_its_own_work_directory() {
    use crate::dispatch::{NodeKind as _, RunnerSnapshot};
    use crate::runner_program::{Interpreter, SandboxProbe, WORK_DIR_NEEDS_SANDBOX};
    use crate::runtime::NodeStart;

    let world = World::new();
    write_definition(world.profile(), "probe", PROBE);
    fs::write(
        world.profile().join("probe").join("check.py"),
        "print('hi')\n",
    )
    .unwrap();
    let (mut lifecycle, worktrees) = counting_lifecycle(&world);
    let other = world
        .start(&mut lifecycle, project_request("probe"))
        .unwrap();
    let run = world
        .start(&mut lifecycle, project_request("probe"))
        .unwrap();
    let project = project_dir(&world);
    let work = project_work_dir(&project, &run);
    let binding = lifecycle.binding(&run).unwrap();

    let seen: Arc<std::sync::Mutex<Vec<(Vec<PathBuf>, PathBuf)>>> = Arc::default();
    let probe: SandboxProbe = {
        let seen = seen.clone();
        Arc::new(move |policy, cwd| {
            let sandbox::SandboxFsPolicy::Restricted { writable_paths } = &policy.fs else {
                panic!("the policy restricts no path: {:?}", policy.fs);
            };
            seen.lock()
                .unwrap()
                .push((writable_paths.clone(), cwd.to_path_buf()));
            Err("no sandbox (test)".to_string())
        })
    };
    let mut kind = crate::dispatch_program::ProgramNodes::with_clock_and_probe(
        Arc::new(StillClock(std::time::Instant::now())),
        probe,
    );
    let snapshot = RunnerSnapshot {
        interpreter: Some(Ok(Interpreter {
            program: "python3".into(),
            version: (3, 8),
        })),
        ..RunnerSnapshot::default()
    };
    let definition = lifecycle.runtime().definition(&run).unwrap().clone();
    let key = NodeKey::new("check", 1);
    let state = RunState::default();
    let failure = kind
        .start(
            &NodeStart {
                run: &run,
                definition: &definition,
                key: &key,
                try_number: 1,
                state: &state,
                budget_left_usd: None,
                binding: Some(&binding),
            },
            &binding,
            &snapshot,
        )
        .unwrap_err();
    // With no OS sandbox the script never runs: nothing else keeps runs/ from it.
    assert_eq!(failure.reason, FailureReason::PermissionDenied);
    assert!(
        failure.detail.starts_with(WORK_DIR_NEEDS_SANDBOX),
        "{}",
        failure.detail
    );

    let seen = seen.lock().unwrap().clone();
    let [(writable, cwd)] = seen.as_slice() else {
        panic!("the sandbox was asked {} times", seen.len());
    };
    assert_eq!(cwd, &work, "the script runs in the work directory");
    assert!(writable.contains(&work), "{writable:?}");
    let runs = project.join("runs");
    let other_work = project_work_dir(&project, &other);
    for path in writable {
        assert!(path.starts_with(&work), "{} is writable", path.display());
        assert!(
            !path.starts_with(&runs) && !runs.starts_with(path),
            "{}",
            path.display()
        );
        assert!(!path.starts_with(&other_work), "{}", path.display());
        assert!(!project.starts_with(path), "{}", path.display());
    }
    // The script never ran, and no git was asked about the work directory.
    assert!(!work.join("changed.txt").exists());
    assert_eq!(worktrees.calls(), 0);
}

#[test]
fn a_lane_directory_inside_the_project_folder_is_refused_for_a_run_with_no_repository() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let project = project_dir(&world);
    for dir in [
        project.join("runs"),
        project.clone(),
        project.join("documents"),
    ] {
        let request = StartRequest {
            lane: Some(LaneRef {
                id: "lane-1".to_string(),
                kind: "build".to_string(),
            }),
            lane_dirs: [("target".to_string(), dir)].into(),
            ..project_request("single")
        };
        let refusal = world.start(&mut lifecycle, request).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
        assert!(refusal.message.contains("overlaps"), "{refusal}");
    }
    assert!(lifecycle.all_runs().is_empty());
    assert!(!project.join(PROJECT_WORK_DIR).exists(), "nothing was made");
}

#[test]
fn a_work_folder_that_is_a_link_is_refused_and_its_target_untouched() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let project = project_dir(&world);
    let elsewhere = world.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(&project).unwrap();
    std::os::unix::fs::symlink(&elsewhere, project.join(PROJECT_WORK_DIR)).unwrap();
    let refusal = world
        .start(&mut lifecycle, project_request("single"))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BadRequest, "{refusal}");
    assert!(lifecycle.all_runs().is_empty());
    assert!(listing(&elsewhere).is_empty());
    assert!(!project.join("runs").exists());
}

/// `write`: one node reporting a document, promised by its success end.
const WRITE: &str = r#"
schema = 1
name = "write"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
output = { doc = { type = "path", outcome = "document" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
outcomes = ["document"]
"#;

#[test]
fn a_document_of_a_project_run_is_copied_out_at_a_free_name_and_listed_shared() {
    let world = World::new();
    write_definition(world.profile(), "write", WRITE);
    let (mut lifecycle, worktrees) = counting_lifecycle(&world);
    lifecycle
        .runtime_mut()
        .runner_mut()
        .script("work", json!({ "doc": "out/report.md" }));
    let project = project_dir(&world);
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("report.md"), "original\n").unwrap();
    fs::write(
        project.join("artifacts.toml"),
        "schema = 1\nkept = \"yes\"\n\n[[artifact]]\npath = \"plan.md\"\ndescription = \"the plan\"\nvisibility = \"human-only\"\n",
    )
    .unwrap();

    let run = world
        .start(&mut lifecycle, project_request("write"))
        .unwrap();
    let work = project_work_dir(&project, &run);
    fs::create_dir_all(work.join("out")).unwrap();
    fs::write(work.join("out").join("report.md"), "from the run\n").unwrap();
    let end = end_work_run(&world, &mut lifecycle, &run);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Succeeded));

    assert_eq!(
        fs::read_to_string(project.join("report.md")).unwrap(),
        "original\n",
        "an existing file is never overwritten"
    );
    assert_eq!(
        fs::read_to_string(project.join("report-2.md")).unwrap(),
        "from the run\n"
    );
    let manifest = bongos_work_model::io::read_artifacts(&project.join("artifacts.toml")).unwrap();
    assert_eq!(
        manifest.extra.get("kept"),
        Some(&toml::Value::String("yes".to_string()))
    );
    let paths: Vec<&str> = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(paths, vec!["plan.md", "report-2.md"]);
    let copied = &manifest.artifacts[1];
    assert_eq!(
        copied.description,
        crate::outcomes::document_description(&run, "work")
    );
    assert_eq!(
        copied.visibility,
        Some(bongos_work_model::Visibility::Shared)
    );

    // Settled again, as after a wait: nothing is copied twice.
    let settled = block_on(project_settlement(&no_git(&world), &end));
    assert_eq!(settled.settlement, crate::runtime::Settlement::Succeed);
    assert!(!project.join("report-3.md").exists());
    assert_eq!(worktrees.calls(), 0);
}

#[test]
fn a_second_document_of_the_same_name_takes_the_next_free_name_and_never_the_work_folder() {
    use crate::outcomes::{ProjectDocument, ProjectDocuments, deliver_project_documents};
    let temp = tempfile::TempDir::new().unwrap();
    let files = temp.path().join("files");
    let tree = temp.path().join("tree");
    fs::create_dir_all(files.join(PROJECT_WORK_DIR)).unwrap();
    fs::create_dir_all(&tree).unwrap();
    fs::write(files.join("report.md"), "original\n").unwrap();
    fs::write(tree.join("report.md"), "new\n").unwrap();
    fs::write(tree.join("work"), "a file named work\n").unwrap();
    fs::write(tree.join("runs"), "a file named runs\n").unwrap();
    let document = |path: &str| ProjectDocument {
        key: NodeKey::new("work", 1),
        root: tree.clone(),
        path: path.to_string(),
    };
    let deliver = |n: u32, paths: &[&str]| {
        deliver_project_documents(&ProjectDocuments {
            files_dir: files.clone(),
            run_id: "run".to_string(),
            documents: paths.iter().map(|path| document(path)).collect(),
            promised: true,
            marker: temp.path().join(format!("marker-{n}")),
        })
        .unwrap()
    };
    assert_eq!(deliver(1, &["report.md"]), vec!["report-2.md"]);
    assert_eq!(deliver(2, &["report.md"]), vec!["report-3.md"]);
    assert_eq!(deliver(3, &["work", "runs"]), vec!["work-2", "runs-2"]);
    assert_eq!(
        fs::read_to_string(files.join("report.md")).unwrap(),
        "original\n"
    );
    assert!(files.join(PROJECT_WORK_DIR).is_dir());
    assert!(!files.join("runs").exists());
    assert_eq!(
        fs::read_to_string(files.join("work-2")).unwrap(),
        "a file named work\n"
    );
}

#[test]
fn a_document_path_that_leaves_its_tree_is_refused_and_nothing_is_written() {
    use crate::outcomes::{ProjectDocument, ProjectDocuments, deliver_project_documents};
    let temp = tempfile::TempDir::new().unwrap();
    let files = temp.path().join("files");
    let tree = files.join(PROJECT_WORK_DIR).join("run");
    fs::create_dir_all(&tree).unwrap();
    let secret = temp.path().join("secret.md");
    fs::write(&secret, "secret\n").unwrap();
    let records = files.join("runs").join("other");
    fs::create_dir_all(&records).unwrap();
    fs::write(records.join("run.toml"), "records\n").unwrap();
    std::os::unix::fs::symlink(&secret, tree.join("link.md")).unwrap();
    std::os::unix::fs::symlink(&records, tree.join("records")).unwrap();
    let before = listing(&files);
    for path in [
        "../../secret.md",
        "../other/x.md",
        secret.to_str().unwrap(),
        "/etc/hostname",
        "link.md",
        "records/run.toml",
        "",
        "missing.md",
    ] {
        let result = deliver_project_documents(&ProjectDocuments {
            files_dir: files.clone(),
            run_id: "run".to_string(),
            documents: vec![ProjectDocument {
                key: NodeKey::new("work", 1),
                root: tree.clone(),
                path: path.to_string(),
            }],
            promised: true,
            marker: temp.path().join("marker"),
        });
        let failure = result.unwrap_err();
        assert_eq!(
            failure.reason,
            FailureReason::OutcomePromisedNotDelivered,
            "{path}"
        );
        assert_eq!(listing(&files), before, "{path} wrote something");
        assert!(!temp.path().join("marker").exists());
    }
}

#[test]
fn artifact_entries_of_runs_ending_at_once_are_all_kept() {
    use crate::outcomes::record_project_artifact;
    let temp = tempfile::TempDir::new().unwrap();
    let files = temp.path().to_path_buf();
    fs::write(
        files.join("artifacts.toml"),
        "schema = 1\n\n[[artifact]]\npath = \"plan.md\"\ndescription = \"the plan\"\n",
    )
    .unwrap();
    let threads: Vec<_> = (0..4)
        .map(|thread| {
            let files = files.clone();
            std::thread::spawn(move || {
                for n in 0..10 {
                    record_project_artifact(&files, &format!("doc-{thread}-{n}.md"), "from a run")
                        .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let manifest = bongos_work_model::io::read_artifacts(&files.join("artifacts.toml")).unwrap();
    assert_eq!(manifest.artifacts.len(), 41);
    assert_eq!(manifest.artifacts[0].path, "plan.md");
    assert_eq!(manifest.artifacts[0].description, "the plan");
    for thread in 0..4 {
        for n in 0..10 {
            let name = format!("doc-{thread}-{n}.md");
            assert!(
                manifest
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.path == name),
                "{name} was lost"
            );
        }
    }
    // No temporary file is left behind.
    assert_eq!(listing(&files), vec![PathBuf::from("artifacts.toml")]);

    // A manifest that does not parse is left as it is.
    fs::write(files.join("artifacts.toml"), "schema = [\n").unwrap();
    assert!(record_project_artifact(&files, "late.md", "from a run").is_err());
    assert_eq!(
        fs::read_to_string(files.join("artifacts.toml")).unwrap(),
        "schema = [\n"
    );
}

#[test]
fn clear_finished_removes_a_finished_run_s_work_directory_and_nothing_else() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().hold_success(true);
    let project = project_dir(&world);

    let finished = world
        .start(&mut lifecycle, project_request("single"))
        .unwrap();
    let finished_work = project_work_dir(&project, &finished);
    fs::create_dir_all(finished_work.join("deep")).unwrap();
    fs::write(finished_work.join("deep").join("out.txt"), "out\n").unwrap();
    // A link inside leads out of the work directory: it goes, its target stays.
    let outside = world.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    std::os::unix::fs::symlink(&outside, finished_work.join("link")).unwrap();
    end_work_run(&world, &mut lifecycle, &finished);
    assert_eq!(lifecycle.status(&finished), Some(RunStatus::Succeeded));

    let active = world
        .start(&mut lifecycle, project_request("single"))
        .unwrap();
    let active_work = project_work_dir(&project, &active);
    fs::write(active_work.join("busy.txt"), "busy\n").unwrap();

    // A snapshot run's scratch is in work/ too, and is not this step's to remove.
    let path = snapshot(&world);
    let snapshot_run = world
        .start(
            &mut lifecycle,
            StartRequest {
                tree: RunTree::Snapshot { path },
                ..project_request("single")
            },
        )
        .unwrap();
    let scratch = project_work_dir(&project, &snapshot_run);
    fs::create_dir_all(&scratch).unwrap();
    fs::write(scratch.join("scratch.txt"), "scratch\n").unwrap();
    lifecycle.cancel(&snapshot_run).unwrap();
    fs::write(project.join("notes.md"), "notes\n").unwrap();

    let outcome = lifecycle.clear_finished();
    assert_eq!(
        sorted(outcome.cleared),
        sorted(vec![finished.clone(), snapshot_run.clone()])
    );
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    assert!(
        !finished_work.exists(),
        "the finished run's work directory is gone"
    );
    assert!(
        project
            .join("runs")
            .join(&finished)
            .join(crate::lifecycle::CLEARED_FILE)
            .exists(),
        "its records are cleared with it"
    );
    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(active_work.join("busy.txt")).unwrap(),
        "busy\n"
    );
    assert_eq!(
        fs::read_to_string(scratch.join("scratch.txt")).unwrap(),
        "scratch\n"
    );
    assert_eq!(
        fs::read_to_string(project.join("notes.md")).unwrap(),
        "notes\n"
    );
    assert!(project.join("runs").join(&active).join("run.toml").exists());
}

#[test]
fn a_work_directory_left_by_a_crash_stays_until_the_run_is_cleared() {
    let world = World::new();
    let project = project_dir(&world);
    let run = {
        let mut lifecycle = world.lifecycle();
        let run = world
            .start(&mut lifecycle, project_request("single"))
            .unwrap();
        fs::write(project_work_dir(&project, &run).join("half.txt"), "half\n").unwrap();
        run
        // The app goes away with the run in flight.
    };
    let work = project_work_dir(&project, &run);

    let mut relaunched = world.lifecycle();
    relaunched.load_runs(&world.tree).unwrap();
    assert!(relaunched.status(&run).is_some_and(RunStatus::is_active));
    assert!(relaunched.clear_finished().cleared.is_empty());
    assert_eq!(fs::read_to_string(work.join("half.txt")).unwrap(), "half\n");

    relaunched.cancel(&run).unwrap();
    assert_eq!(relaunched.clear_finished().cleared, vec![run.clone()]);
    assert!(!work.exists());
    assert!(
        project.join(PROJECT_WORK_DIR).is_dir(),
        "only the run's own directory goes"
    );
}

#[test]
fn removing_a_work_directory_refuses_every_path_but_the_run_s_own() {
    let temp = tempfile::TempDir::new().unwrap();
    let project = temp.path().join("project");
    let work = project.join(PROJECT_WORK_DIR);
    fs::create_dir_all(work.join("run-a")).unwrap();
    fs::write(project.join("notes.md"), "notes\n").unwrap();
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "keep\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, work.join("run-link")).unwrap();
    fs::write(work.join("run-file"), "a file\n").unwrap();
    let before = listing(temp.path());

    for id in [
        "",
        ".",
        "..",
        "../project",
        "run-a/..",
        "a/b",
        "/abs",
        "run-a/",
    ] {
        assert!(remove_project_work_dir(&project, id).is_err(), "'{id}'");
    }
    // A run directory that is a link or a file is refused; one that is gone is nothing to do.
    assert!(remove_project_work_dir(&project, "run-link").is_err());
    assert!(remove_project_work_dir(&project, "run-file").is_err());
    assert!(!remove_project_work_dir(&project, "run-gone").unwrap());
    assert_eq!(listing(temp.path()), before);

    // A work folder that is a link is refused whole.
    let linked = temp.path().join("linked");
    fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(
        &project.join(PROJECT_WORK_DIR),
        linked.join(PROJECT_WORK_DIR),
    )
    .unwrap();
    assert!(remove_project_work_dir(&linked, "run-a").is_err());
    assert!(work.join("run-a").is_dir());

    assert!(remove_project_work_dir(&project, "run-a").unwrap());
    assert!(!work.join("run-a").exists());
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(project.join("notes.md")).unwrap(),
        "notes\n"
    );
}
