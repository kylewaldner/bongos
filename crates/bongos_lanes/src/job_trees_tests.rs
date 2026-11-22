//! The trees of a lane job: the extra repositories other features name, the lane tree or fresh
//! tree each gets, the map of them, and the refusals that leave the lane waiting.

use std::path::PathBuf;
use std::sync::Mutex;

use bongos_lanes_model::{JobRecord, LaneId, RunSubject, TaskId, WaitReason};
use bongos_work_model::store::WorkTree;
use bongos_workflow::worktrees::run_branch_name;
use gpui::TestAppContext;

use crate::job_trees::{
    CurrentJobTrees, JobRepositories, JobRepository, JobStart, JobTrees, start_job_trees,
};
use crate::lane_tree_tests::{Fixture, RUN_1, RUN_2, TASK_BRANCH, git, last};
use crate::lane_tree::LastEnd;

const BOTH_TABLE: &str = r#"schema = 1

[[kind]]
name = "build"
count = 2
[kind.binding]
dirs = ["target"]
trees = [
  { repo = "app", mode = "persistent" },
  { repo = "lib", mode = "persistent" },
]
"#;

fn job() -> JobRecord {
    JobRecord {
        run_id: RUN_1.to_string(),
        subject: RunSubject::Task(TaskId::new("leaf")),
        workflow: "build".to_string(),
        started: "2026-03-01T10:00:00Z".to_string(),
        definition_commit: None,
    }
}

fn lib_too(_: &JobRecord, _: &WorkTree) -> Vec<String> {
    vec!["lib".to_string(), "app".to_string(), "lib".to_string()]
}

fn lanes() -> Vec<LaneId> {
    vec![LaneId::new("build", 1), LaneId::new("build", 2)]
}

fn start(fixture: &Fixture, run_id: &str, extra: Vec<JobRepository>) -> JobStart {
    JobStart {
        locations: fixture.locations.clone(),
        table_lanes: lanes(),
        primary: fixture.task_job(run_id, None),
        extra,
        save_max_file_bytes: 1000,
        forbidden: fixture.forbidden(),
    }
}

fn lib_job(fixture: &Fixture, run_id: &str) -> JobRepository {
    JobRepository::for_project("lib", fixture.lib.clone(), None, "main", run_id, 1, None)
}

#[gpui::test]
async fn a_registered_extra_repository_gets_its_tree_in_the_job_trees(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    assert!(
        cx.update(|cx| JobRepositories::extra(&job(), &WorkTree::default(), "app", cx))
            .is_empty(),
        "none registered: none"
    );
    cx.update(|cx| JobRepositories::register(lib_too, cx));
    let extra = cx.update(|cx| JobRepositories::extra(&job(), &WorkTree::default(), "app", cx));
    assert_eq!(extra, ["lib"]);

    // The binding lists both repositories: each runs in the lane's own tree of it.
    let fixture = Fixture::build(BOTH_TABLE, |_| None);
    let fresh_asked = Mutex::new(Vec::new());
    let fresh = async |repository: &JobRepository| -> Result<PathBuf, String> {
        fresh_asked.lock().unwrap().push(repository.name.clone());
        Err("no fresh tree here".to_string())
    };
    let started = start_job_trees(
        &fixture.git,
        &start(&fixture, RUN_1, vec![lib_job(&fixture, RUN_1)]),
        &fresh,
    )
    .await
    .unwrap();
    let app_tree = fixture.locations.get("tree:app").unwrap().path.clone();
    let lib_tree = fixture.locations.get("tree:lib").unwrap().path.clone();
    assert_eq!(started.trees.primary_repo(), "app");
    assert_eq!(started.trees.primary(), app_tree);
    assert_eq!(started.trees.tip("lib"), Some(lib_tree.as_path()));
    assert!(fresh_asked.lock().unwrap().is_empty());
    assert_eq!(
        git(&lib_tree, &["branch", "--show-current"]),
        run_branch_name("main", RUN_1, 1)
    );
    assert_eq!(
        git(&app_tree, &["branch", "--show-current"]),
        run_branch_name(TASK_BRANCH, RUN_1, 1)
    );

    // Kept per lane for the current job.
    let lane = LaneId::new("build", 1);
    cx.update(|cx| CurrentJobTrees::set(lane.clone(), started.trees.clone(), cx));
    assert_eq!(
        cx.update(|cx| CurrentJobTrees::of(&lane, cx)),
        Some(started.trees.clone())
    );
    cx.update(|cx| CurrentJobTrees::clear(&lane, cx));
    assert_eq!(cx.update(|cx| CurrentJobTrees::of(&lane, cx)), None);
}

#[gpui::test]
async fn a_repository_the_binding_does_not_list_gets_a_fresh_tree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let fresh_tree = fixture.base.join("fresh/lib");
    let fresh_asked = Mutex::new(Vec::new());
    let fresh = async |repository: &JobRepository| -> Result<PathBuf, String> {
        fresh_asked.lock().unwrap().push(repository.name.clone());
        Ok(fresh_tree.clone())
    };
    let started = start_job_trees(
        &fixture.git,
        &start(&fixture, RUN_1, vec![lib_job(&fixture, RUN_1)]),
        &fresh,
    )
    .await
    .unwrap();
    assert_eq!(*fresh_asked.lock().unwrap(), ["lib"]);
    assert_eq!(JobTrees::new("app", fixture.tip()).tip("lib"), None);
    assert_eq!(started.trees.primary(), fixture.tip());
    assert_eq!(started.trees.tip("lib"), Some(fresh_tree.as_path()));
    let repos: Vec<&str> = started.trees.iter().map(|(repo, _)| repo).collect();
    assert_eq!(repos, ["app", "lib"]);
}

#[gpui::test]
async fn a_failing_reset_or_a_lane_out_of_the_table_waits_and_deletes_nothing(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let fixture = Fixture::new();
    let tip = fixture.tip();
    let fresh = async |_: &JobRepository| -> Result<PathBuf, String> { Err("unused".to_string()) };
    start_job_trees(&fixture.git, &start(&fixture, RUN_1, Vec::new()), &fresh)
        .await
        .unwrap();
    std::fs::write(tip.join("new.txt"), "left by the run\n").unwrap();

    // A lane removed from the table is never reset.
    let mut removed = start(&fixture, RUN_2, Vec::new());
    removed.table_lanes = vec![LaneId::new("build", 2)];
    let reason = start_job_trees(&fixture.git, &removed, &fresh)
        .await
        .unwrap_err();
    assert!(
        matches!(&reason, WaitReason::Environment(text) if text.contains("not in the lane table")),
        "{reason:?}"
    );
    assert!(tip.join("new.txt").exists());

    // A git command of the reset fails: an environment wait, and nothing deleted.
    let lock = PathBuf::from(git(
        &tip,
        &["rev-parse", "--path-format=absolute", "--git-path", "index.lock"],
    ));
    std::fs::write(&lock, "").unwrap();
    let mut failed = start(&fixture, RUN_2, Vec::new());
    failed.primary.last = Some(last(RUN_1, LastEnd::Succeeded));
    let reason = start_job_trees(&fixture.git, &failed, &fresh)
        .await
        .unwrap_err();
    assert!(
        matches!(&reason, WaitReason::Environment(text) if text.starts_with("build-1: ")),
        "{reason:?}"
    );
    assert!(tip.join("new.txt").exists());
    assert_eq!(
        git(&tip, &["branch", "--show-current"]),
        run_branch_name(TASK_BRANCH, RUN_1, 1)
    );
    std::fs::remove_file(&lock).unwrap();
}
