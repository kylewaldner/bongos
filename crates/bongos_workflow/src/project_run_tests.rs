//! Tests of runs with no task and of lane runs: a project run on a repository works in a run
//! tree made from its base on its own run branch and merges into its target only at its success
//! end, and when another worktree has the target checked out it waits on a person with the run
//! branch and that checkout unchanged; a run on a snapshot works in the snapshot with no tree or
//! branch of its own; an agent of a project run is told about the project, never a task; a lane
//! run carries its lane, directories and environment edits, and a start with none of them is
//! set up as a task run always was. Real git, isolated from the real configuration, only where
//! the test is about git; the runs are the scripted ones of the lifecycle tests, a child of which
//! this is.

use futures::executor::block_on;

use super::*;
use crate::builtin_merge::ProjectMergeRequest;
use crate::lifecycle::{ProjectEnd, project_settlement, project_work_dir};
use crate::runtime::RunSetup;
use crate::task_link::{MERGE_GATE, gate_key};
use crate::worktrees::run_branch_name;
use bongos_workflow_model::{EnvEdit, RunTreeRecord};

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
    let request = |merge_into: &str| ProjectMergeRequest {
        run_tree: tree.clone(),
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
    };
    let settled = block_on(project_settlement(&Git::new("git"), &end));
    assert_eq!(settled.settlement, crate::runtime::Settlement::Succeed);
    assert_eq!(settled.merged_head, None);
    // A merge already recorded is never made again.
    let end = ProjectEnd {
        merge: Some(ProjectMergeRequest {
            run_tree: PathBuf::from("/nowhere"),
            run_branch: "run".to_string(),
            merge_into: "main".to_string(),
            run_id: "run".to_string(),
            workflow: "single".to_string(),
        }),
        merged: true,
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
