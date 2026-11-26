//! Fixture trees, fake live facts and a deterministic id source for the tests.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use bongos_project_repo_model::{DefinitionTree, RelPath};
use bongos_work_model::TaskId;
use bongos_workflow_model::definition::Scope;
use serde_json::Value;

use crate::apply::{Applied, ApplyCtx, EditRefusal, StartedFacts, apply};
use crate::ops::GraphEditBatch;

pub const PROJECT: &str = "schema = 1\nid = \"p\"\ntitle = \"P\"\n";

/// The `task.toml` of a task with id `id` and goal `goal`, followed by `extra` lines.
pub fn task_toml(id: &str, goal: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// The files of the fixture: the top group `top`, the run's task `run` with the children `a`,
/// `b` (after `a`) and `c`, and `other` with the child `x`. Each task's goal is `Goal <id>`.
pub fn fixture_files() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    files.insert("project.toml".to_string(), PROJECT.to_string());
    files.insert("task.toml".to_string(), task_toml("top", "Top", ""));
    let tasks = [
        ("tasks/run", "run", ""),
        ("tasks/run/tasks/a", "a", ""),
        (
            "tasks/run/tasks/b",
            "b",
            "after = [{ task = \"a\", unblock = \"merged\" }]\n",
        ),
        ("tasks/run/tasks/c", "c", ""),
        ("tasks/other", "other", ""),
        ("tasks/other/tasks/x", "x", ""),
    ];
    for (folder, id, extra) in tasks {
        files.insert(
            format!("{folder}/task.toml"),
            task_toml(id, &format!("Goal {id}"), extra),
        );
    }
    files
}

pub fn parse(files: &BTreeMap<String, String>) -> DefinitionTree {
    DefinitionTree::parse_files(files.iter().map(|(path, text)| {
        (
            RelPath::new(path.clone()).expect("a definitions path"),
            text.clone().into_bytes(),
        )
    }))
    .expect("the fixture parses")
}

pub fn fixture() -> DefinitionTree {
    parse(&fixture_files())
}

/// Live facts by task id.
#[derive(Default)]
pub struct Facts {
    pub started: BTreeSet<String>,
    pub held: BTreeMap<String, String>,
    pub runs: BTreeSet<String>,
}

impl Facts {
    pub fn started(ids: &[&str]) -> Self {
        Self {
            started: ids.iter().map(|id| id.to_string()).collect(),
            ..Self::default()
        }
    }
}

impl StartedFacts for Facts {
    fn started(&self, task: &str) -> bool {
        self.started.contains(task)
    }

    fn held(&self, task: &str) -> Option<String> {
        self.held.get(task).cloned()
    }

    fn has_runs(&self, task: &str) -> bool {
        self.runs.contains(task)
    }
}

/// The `n`th id the test id source hands out: a UUID v7 at a fixed time.
pub fn nth_id(n: u64) -> TaskId {
    format!("019a2b3c-4d5e-7000-8000-{n:012x}")
}

/// Applies the batch `ops` (JSON) to `tree` as the run on `run` with `scope`.
pub fn run_with(
    tree: &DefinitionTree,
    batch: Value,
    scope: Scope,
    facts: &Facts,
    refs: &BTreeMap<String, TaskId>,
) -> Result<Applied, EditRefusal> {
    let counter = Cell::new(0u64);
    let new_id = || {
        let n = counter.get() + 1;
        counter.set(n);
        nth_id(n)
    };
    let ctx = ApplyCtx {
        project_id: "p".to_string(),
        run_task: "run".to_string(),
        scope,
        live: facts,
        new_id: &new_id,
        refs,
    };
    let batch: GraphEditBatch = serde_json::from_value(batch).expect("the batch parses");
    apply(tree, &batch, &ctx)
}

/// [`run_with`] on the fixture, own-subtree scope, nothing started, no recorded refs.
pub fn run(batch: Value) -> Result<Applied, EditRefusal> {
    run_with(
        &fixture(),
        batch,
        Scope::OwnSubtree,
        &Facts::default(),
        &BTreeMap::new(),
    )
}

/// The refusal text of a batch that must be refused.
pub fn refusal(batch: Value, scope: Scope, facts: &Facts) -> String {
    run_with(&fixture(), batch, scope, facts, &BTreeMap::new())
        .expect_err("the batch is refused")
        .to_string()
}
