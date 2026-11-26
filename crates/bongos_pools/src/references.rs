//! The references between folders that a landing checks (`pool-references`): every pool a lane
//! binding, a hook, a workflow node or a check names is declared; every queue, workflow, task and
//! item field a `pulls` entry or a trigger names exists; and a workflow that may run with no task
//! reads no task input and merges only where the run has a tree and a branch to merge into.
//!
//! [`reference_problems`] is pure over the texts of the change's definitions; [`read_inputs`]
//! reads them from a materialized tree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bongos_checks_model::decl::{NeedEntry, parse_file as parse_check_file};
use bongos_lanes_model::parse_accepting as parse_lane_table;
use bongos_pools_model::{
    Action, AllDone, On, PoolDecl, Problem, QueueDecl, TriggerDecl, lane_fields, parse_need,
    parse_pools, parse_queues, parse_triggers,
};
use bongos_project_repo_model::DefinitionTree;
use bongos_project_repo_model::layout::{TASK_FILE, WORKFLOW_FILE, WORKFLOWS_DIR};
use bongos_workflow_model::{Definition, LayerDirs, parse as parse_workflow};

pub const POOLS_FOLDER: &str = "pools";
pub const QUEUES_FOLDER: &str = "queues";
pub const TRIGGERS_FOLDER: &str = "triggers";
pub const POOLS_FILE: &str = "pools/pools.toml";
pub const QUEUES_FILE: &str = "queues/queues.toml";
pub const TRIGGERS_FILE: &str = "triggers/triggers.toml";
pub const LANE_TABLE: &str = "lanes/lanes.toml";
/// The lane table keys this feature reads.
pub const LANE_TABLE_KEYS: [&str; 3] = ["kind.pulls", "kind.binding.pools", "kind.binding.hooks"];

/// The text of one workflow definition and the name its problems are reported under
/// (`workflows/<folder>` for the project's own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowText {
    pub label: String,
    pub text: String,
}

/// What the references are checked against: the texts of one tree of definitions. A missing file
/// is the empty text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceInputs {
    pub pools: String,
    pub queues: String,
    pub triggers: String,
    pub lanes: String,
    /// The project's workflows (`workflows/*/workflow.toml`) in the tree.
    pub project_workflows: Vec<WorkflowText>,
    /// The user's and the shipped workflows by name; `None` when they are not known, and then
    /// every name the project does not define resolves.
    pub other_workflows: Option<BTreeMap<String, WorkflowText>>,
    /// Files with check declarations, by path relative to `definitions/`: the `task.toml` of each
    /// task with checks, and the named sets.
    pub check_files: Vec<(String, String)>,
    /// The ids of the tasks in the tree; `None` when the tasks do not parse.
    pub task_ids: Option<BTreeSet<String>>,
}

fn problem(file: &str, line: usize, message: impl Into<String>) -> Problem {
    Problem {
        file: file.to_string(),
        line,
        message: message.into(),
    }
}

/// How a workflow name resolves.
enum Resolution<'a> {
    Missing,
    /// It resolves, but its definition cannot be read here.
    Unread,
    Found(&'a str, &'a Definition),
}

struct Workflows {
    project: BTreeMap<String, (String, Option<Definition>)>,
    others: Option<BTreeMap<String, (String, Option<Definition>)>>,
}

impl Workflows {
    fn new(inputs: &ReferenceInputs) -> Self {
        let read = |workflow: &WorkflowText| match parse_workflow(&workflow.text) {
            Ok(definition) => (
                definition.name.clone(),
                (workflow.label.clone(), Some(definition)),
            ),
            // A definition that does not parse is refused by the workflow check; its folder
            // name keeps the names that point at it from being reported as well.
            Err(_) => (
                workflow
                    .label
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                (workflow.label.clone(), None),
            ),
        };
        Self {
            project: inputs.project_workflows.iter().map(read).collect(),
            others: inputs.other_workflows.as_ref().map(|others| {
                others
                    .iter()
                    .map(|(name, workflow)| {
                        let (_, entry) = read(workflow);
                        (name.clone(), entry)
                    })
                    .collect()
            }),
        }
    }

    fn resolve(&self, name: &str) -> Resolution<'_> {
        fn found(entry: &(String, Option<Definition>)) -> Resolution<'_> {
            match entry {
                (label, Some(definition)) => Resolution::Found(label.as_str(), definition),
                (_, None) => Resolution::Unread,
            }
        }
        if let Some(entry) = self.project.get(name) {
            return found(entry);
        }
        match &self.others {
            None => Resolution::Unread,
            Some(others) => others.get(name).map_or(Resolution::Missing, found),
        }
    }
}

/// Whether an item of `queue` may have no task: it has no `task_field`, or that field is optional.
pub fn items_may_lack_task(queue: &QueueDecl) -> bool {
    match &queue.task_field {
        None => true,
        Some(field) => queue
            .item
            .iter()
            .find(|item| &item.name == field)
            .is_none_or(|item| item.optional),
    }
}

/// The checks against the declared pools: each `needs` entry names a declared pool and asks for
/// what that pool can give.
struct Pools(Option<BTreeMap<String, PoolDecl>>);

impl Pools {
    fn check(&self, file: &str, line: usize, entry: &toml::Value, out: &mut Vec<Problem>) {
        let Some(pools) = &self.0 else {
            return;
        };
        let need = match parse_need(entry) {
            Ok(need) => need,
            Err(message) => {
                out.push(problem(file, line, message));
                return;
            }
        };
        match pools.get(&need.pool) {
            None => out.push(problem(
                file,
                line,
                format!("pool `{}` is not declared in {POOLS_FILE}", need.pool),
            )),
            Some(pool) => {
                if let Err(message) = need.check_against(pool) {
                    out.push(problem(file, line, message));
                }
            }
        }
    }

    /// Each entry of a `needs` value: an array of entries, or one entry.
    fn check_all(&self, file: &str, line: usize, needs: &toml::Value, out: &mut Vec<Problem>) {
        match needs {
            toml::Value::Array(entries) => {
                for entry in entries {
                    self.check(file, line, entry, out);
                }
            }
            entry => self.check(file, line, entry, out),
        }
    }
}

struct Queues(Option<BTreeMap<String, QueueDecl>>);

impl Queues {
    /// The queue `name`, or a problem when it is not declared. `Ok(None)` when the queues are not
    /// known (their file does not parse).
    fn get(
        &self,
        file: &str,
        line: usize,
        name: &str,
        out: &mut Vec<Problem>,
    ) -> Option<&QueueDecl> {
        let queues = self.0.as_ref()?;
        let queue = queues.get(name);
        if queue.is_none() {
            out.push(problem(
                file,
                line,
                format!("queue `{name}` is not declared in {QUEUES_FILE}"),
            ));
        }
        queue
    }
}

fn check_task(
    task_ids: Option<&BTreeSet<String>>,
    file: &str,
    line: usize,
    trigger: &str,
    task: &str,
    out: &mut Vec<Problem>,
) {
    if let Some(ids) = task_ids
        && !ids.contains(task)
    {
        out.push(problem(
            file,
            line,
            format!("trigger `{trigger}`: task `{task}` is not in the definitions"),
        ));
    }
}

/// The item field an input source reads (`item.<field>`), if it reads one.
fn item_field(source: &str) -> Option<&str> {
    source.strip_prefix("item.")
}

fn check_item_inputs<'a>(
    inputs: impl Iterator<Item = (&'a str, &'a str)>,
    queue: Option<&QueueDecl>,
    file: &str,
    line: usize,
    out: &mut Vec<Problem>,
) {
    for (input, source) in inputs {
        let Some(field) = item_field(source) else {
            continue;
        };
        match queue {
            Some(queue) if !queue.item.iter().any(|item| item.name == field) => {
                out.push(problem(
                    file,
                    line,
                    format!(
                        "input `{input}` reads item.{field}, which is not an item field of queue `{}`",
                        queue.name
                    ),
                ));
            }
            _ => {}
        }
    }
}

/// Where a run with no task comes from, for the messages of [`check_project_subject`].
struct NoTaskRun<'a> {
    file: &'a str,
    line: usize,
    /// The inputs the entry sets itself; a task input it sets is not read from the task.
    inputs: BTreeSet<&'a str>,
    /// Whether the run has a code tree: a trigger's run without a task has one only with `repo`;
    /// a lane job runs in its lane's tree or in a fresh one for `repo`.
    has_tree: bool,
    merge_into: bool,
    /// Why the run may have no task, ending the problem of a task input.
    why: String,
    /// What a merge node without `merge_into` lacks.
    no_branch: &'static str,
}

fn check_project_subject(
    label: &str,
    definition: &Definition,
    run: &NoTaskRun,
    out: &mut Vec<Problem>,
) {
    // One problem per input that reads the task, in declaration order.
    let task_inputs = definition
        .inputs
        .iter()
        .filter(|input| bongos_workflow_model::inputs::reads_task(&input.from));
    for (input, diagnostic) in
        task_inputs.zip(bongos_workflow_model::project_subject_problems(definition))
    {
        if !run.inputs.contains(input.name.as_str()) {
            out.push(problem(
                label,
                diagnostic.line,
                format!("{}; {}", diagnostic.message, run.why),
            ));
        }
    }
    for node in &definition.nodes {
        let Some(use_node) = &node.use_node else {
            continue;
        };
        if use_node.value != "merge" {
            continue;
        }
        if !run.has_tree {
            out.push(problem(
                label,
                use_node.line,
                "a run with no task and no repo has no tree to merge",
            ));
        } else if !run.merge_into {
            out.push(problem(
                label,
                use_node.line,
                format!(
                    "a merge node needs merge_into on {} line {}, because {}",
                    run.file, run.line, run.no_branch
                ),
            ));
        }
    }
}

fn text_of<'a>(table: &'a toml::Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(toml::Value::as_str)
}

fn check_lane_table(
    inputs: &ReferenceInputs,
    pools: &Pools,
    queues: &Queues,
    workflows: &Workflows,
    out: &mut Vec<Problem>,
) {
    if inputs.lanes.trim().is_empty() {
        return;
    }
    // A table that does not parse is refused by the lanes folder check.
    let Ok(kinds) = lane_fields(&inputs.lanes) else {
        return;
    };
    let trees: BTreeMap<String, bool> = parse_lane_table(&inputs.lanes, &LANE_TABLE_KEYS)
        .map(|table| {
            table
                .kinds
                .iter()
                .map(|kind| {
                    let has_trees = kind
                        .binding
                        .as_ref()
                        .is_some_and(|binding| !binding.trees.is_empty());
                    (kind.name.clone(), has_trees)
                })
                .collect()
        })
        .unwrap_or_default();
    for (kind, fields) in &kinds {
        for entry in &fields.pools {
            pools.check(LANE_TABLE, entry.line, &entry.value, out);
        }
        for (hook, entry) in &fields.hooks {
            match entry.value.as_table() {
                Some(table) => {
                    if let Some(needs) = table.get("needs") {
                        pools.check_all(LANE_TABLE, entry.line, needs, out);
                    }
                }
                None => out.push(problem(
                    LANE_TABLE,
                    entry.line,
                    format!(
                        "hook {hook} must be a table like {{ run = \"lanes/hooks/<script>\" }}"
                    ),
                )),
            }
        }
        for entry in &fields.pulls {
            let has_trees = trees.get(kind).copied().unwrap_or(false);
            check_pulls_entry(
                kind,
                has_trees,
                entry.line,
                &entry.value,
                queues,
                workflows,
                out,
            );
        }
    }
}

fn check_pulls_entry(
    kind: &str,
    has_trees: bool,
    line: usize,
    value: &toml::Value,
    queues: &Queues,
    workflows: &Workflows,
    out: &mut Vec<Problem>,
) {
    let Some(table) = value.as_table() else {
        out.push(problem(
            LANE_TABLE,
            line,
            "a pulls entry is a table like { queue = \"<queue>\", workflow = \"<workflow>\" }",
        ));
        return;
    };
    let Some(queue_name) = text_of(table, "queue") else {
        out.push(problem(LANE_TABLE, line, "a pulls entry needs a queue"));
        return;
    };
    let queue = queues.get(LANE_TABLE, line, queue_name, out);
    let inputs: Vec<(&str, &str)> = table
        .get("inputs")
        .and_then(toml::Value::as_table)
        .map(|inputs| {
            inputs
                .iter()
                .filter_map(|(input, source)| Some((input.as_str(), source.as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    check_item_inputs(inputs.iter().copied(), queue, LANE_TABLE, line, out);

    let resolved =
        text_of(table, "workflow").map(|workflow| (workflow, workflows.resolve(workflow)));
    if let Some((workflow, Resolution::Missing)) = &resolved {
        out.push(problem(
            LANE_TABLE,
            line,
            format!(
                "workflow `{workflow}` does not resolve in the project, user or shipped workflows"
            ),
        ));
    }
    let Some(queue) = queue else {
        return;
    };
    if !items_may_lack_task(queue) {
        return;
    }
    let repo = text_of(table, "repo").is_some();
    if !has_trees && !repo {
        out.push(problem(
            LANE_TABLE,
            line,
            format!("kind {kind} has no lane tree; this pulls entry needs repo"),
        ));
    }
    if let Some((_, Resolution::Found(label, definition))) = resolved {
        let run = NoTaskRun {
            file: LANE_TABLE,
            line,
            inputs: inputs.iter().map(|(input, _)| *input).collect(),
            has_tree: true,
            merge_into: text_of(table, "merge_into").is_some(),
            why: format!(
                "{LANE_TABLE} line {line} runs it for items of queue `{}`, which may have no task",
                queue.name
            ),
            no_branch: "an item without a task has no task branch",
        };
        check_project_subject(label, definition, &run, out);
    }
}

fn check_trigger(
    trigger: &TriggerDecl,
    queues: &Queues,
    workflows: &Workflows,
    task_ids: Option<&BTreeSet<String>>,
    out: &mut Vec<Problem>,
) {
    let on_line = trigger.key_line("on");
    let action_line = trigger.key_line("action");
    let name = trigger.name.as_str();
    let mut on_queue = None;
    match &trigger.on {
        On::Queue { queue, consume } => {
            on_queue = queues
                .get(TRIGGERS_FILE, on_line, queue, out)
                .map(|queue| (queue, *consume));
        }
        On::TaskDone(task) | On::AllDone(AllDone::Under(task)) => {
            check_task(task_ids, TRIGGERS_FILE, on_line, name, task, out);
        }
        On::Timer(_) | On::AllDone(AllDone::WorkKind(_)) | On::RunEnded { .. } => {}
    }
    match &trigger.action {
        Action::Dispatcher => {}
        Action::Push { queue, fields } => {
            if let Some(queue) = queues.get(TRIGGERS_FILE, action_line, queue, out) {
                for field in fields.keys() {
                    if !queue.item.iter().any(|item| &item.name == field) {
                        out.push(problem(
                            TRIGGERS_FILE,
                            action_line,
                            format!(
                                "trigger `{name}`: push field `{field}` is not an item field of queue `{}`",
                                queue.name
                            ),
                        ));
                    }
                }
            }
        }
        Action::Workflow {
            workflow,
            task,
            repo,
            base,
            merge_into,
            inputs,
        } => {
            if let Some(task) = task {
                check_task(task_ids, TRIGGERS_FILE, action_line, name, task, out);
            }
            if repo.is_none() && (base.is_some() || merge_into.is_some()) {
                out.push(problem(
                    TRIGGERS_FILE,
                    action_line,
                    "base and merge_into need repo",
                ));
            }
            let reads_item = inputs
                .iter()
                .filter(|(_, source)| item_field(source).is_some());
            match on_queue {
                Some((queue, _)) => check_item_inputs(
                    inputs
                        .iter()
                        .map(|(input, source)| (input.as_str(), source.as_str())),
                    Some(queue),
                    TRIGGERS_FILE,
                    action_line,
                    out,
                ),
                None if matches!(trigger.on, On::Queue { .. }) => {}
                None => {
                    for (input, source) in reads_item {
                        out.push(problem(
                            TRIGGERS_FILE,
                            action_line,
                            format!(
                                "trigger `{name}`: input `{input}` reads {source}, but the trigger is not on a queue"
                            ),
                        ));
                    }
                }
            }
            let resolution = workflows.resolve(workflow);
            if let Resolution::Missing = resolution {
                out.push(problem(
                    TRIGGERS_FILE,
                    action_line,
                    format!(
                        "trigger `{name}`: workflow `{workflow}` does not resolve in the project, user or shipped workflows"
                    ),
                ));
            }
            // A consuming trigger on a queue whose items always have a task runs on that task.
            let item_task = matches!(
                on_queue,
                Some((queue, true)) if !items_may_lack_task(queue)
            );
            if task.is_some() || item_task {
                return;
            }
            if let Resolution::Found(label, definition) = resolution {
                let run = NoTaskRun {
                    file: TRIGGERS_FILE,
                    line: action_line,
                    inputs: inputs.keys().map(String::as_str).collect(),
                    has_tree: repo.is_some(),
                    merge_into: merge_into.is_some(),
                    why: format!(
                        "trigger `{name}` on {TRIGGERS_FILE} line {action_line} runs it with no task"
                    ),
                    no_branch: "a run without a task has no task branch",
                };
                check_project_subject(label, definition, &run, out);
            }
        }
    }
}

fn check_entry_value(entry: &NeedEntry) -> Option<toml::Value> {
    match entry {
        NeedEntry::Pool(pool) => Some(toml::Value::String(pool.clone())),
        NeedEntry::Request { pool, rest } => {
            let mut table = rest.clone();
            table.insert("pool".to_string(), toml::Value::String(pool.clone()));
            Some(toml::Value::Table(table))
        }
        // The checks' own validation reports an entry of neither form.
        NeedEntry::Invalid(_) => None,
    }
}

/// Every problem of the references in `inputs`, sorted by file and line. Nothing declared and
/// nothing named is no problem. A declaration file that does not parse is the per-folder check's
/// to report; the references to what it would declare are not judged.
pub fn reference_problems(inputs: &ReferenceInputs) -> Vec<Problem> {
    let pools = Pools(parse_pools(&inputs.pools).ok().map(|parsed| {
        parsed
            .decls
            .into_iter()
            .map(|pool| (pool.name.clone(), pool))
            .collect()
    }));
    let queues = Queues(parse_queues(&inputs.queues).ok().map(|parsed| {
        parsed
            .decls
            .into_iter()
            .map(|queue| (queue.name.clone(), queue))
            .collect()
    }));
    let triggers = parse_triggers(&inputs.triggers)
        .map(|parsed| parsed.decls)
        .unwrap_or_default();
    let workflows = Workflows::new(inputs);
    let mut out = Vec::new();

    check_lane_table(inputs, &pools, &queues, &workflows, &mut out);

    for (label, definition) in workflows.project.values() {
        let Some(definition) = definition else {
            continue;
        };
        for node in &definition.nodes {
            if let Some(needs) = &node.needs {
                pools.check_all(label, needs.line, &needs.value, &mut out);
            }
        }
    }

    for (path, text) in &inputs.check_files {
        // A file that does not parse is refused by the checks' own validation.
        let Ok(table) = parse_check_file(path, text) else {
            continue;
        };
        for decl in table.check.iter().chain(&table.closing) {
            let line = decl.line.unwrap_or(1);
            for entry in &decl.needs {
                if let Some(value) = check_entry_value(entry) {
                    pools.check(path, line, &value, &mut out);
                }
            }
        }
    }

    for trigger in &triggers {
        check_trigger(
            trigger,
            &queues,
            &workflows,
            inputs.task_ids.as_ref(),
            &mut out,
        );
    }

    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then_with(|| a.message.cmp(&b.message))
    });
    out.dedup();
    out
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The workflow folders under `dir`, sorted, as `(folder name, text of workflow.toml)`.
fn workflow_folders(dir: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(folder) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Ok(text) = std::fs::read_to_string(path.join(WORKFLOW_FILE)) {
            found.push((folder.to_string(), text));
        }
    }
    found.sort();
    found
}

/// Reads the inputs of [`reference_problems`] from a `definitions/` folder. `other_layers` are the
/// user's and shipped workflow folders; its project layer is ignored, since the project's
/// workflows are read from `definitions`.
pub fn read_inputs(definitions: &Path, other_layers: Option<LayerDirs>) -> ReferenceInputs {
    let project_workflows = workflow_folders(&definitions.join(WORKFLOWS_DIR))
        .into_iter()
        .map(|(folder, text)| WorkflowText {
            label: format!("{WORKFLOWS_DIR}/{folder}"),
            text,
        })
        .collect();
    let other_workflows = other_layers.map(|mut dirs| {
        dirs.project = None;
        bongos_workflow_model::layers::resolve(&dirs)
            .resolved
            .into_iter()
            .filter_map(|resolved| {
                let text = std::fs::read_to_string(resolved.dir.join(WORKFLOW_FILE)).ok()?;
                let label = resolved.dir.to_string_lossy().into_owned();
                Some((resolved.name, WorkflowText { label, text }))
            })
            .collect()
    });

    let mut check_files = Vec::new();
    let sets = definitions.join("checks").join("sets");
    if let Ok(entries) = std::fs::read_dir(&sets) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "toml") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path) {
                check_files.push((format!("checks/sets/{name}"), text));
            }
        }
    }
    let tree = DefinitionTree::parse_dir(definitions).ok();
    let task_ids = tree.as_ref().map(|tree| {
        tree.nodes()
            .into_iter()
            .map(|node| node.definition.id.clone())
            .collect()
    });
    if let Some(tree) = &tree {
        for node in tree.nodes() {
            if !node.definition.extra.contains_key("checks") {
                continue;
            }
            let file = if node.folder.is_empty() {
                TASK_FILE.to_string()
            } else {
                format!("{}/{TASK_FILE}", node.folder)
            };
            if let Ok(text) = std::fs::read_to_string(definitions.join(&file)) {
                check_files.push((file, text));
            }
        }
    }
    check_files.sort();

    ReferenceInputs {
        pools: read_or_empty(&definitions.join(POOLS_FILE)),
        queues: read_or_empty(&definitions.join(QUEUES_FILE)),
        triggers: read_or_empty(&definitions.join(TRIGGERS_FILE)),
        lanes: read_or_empty(&definitions.join(LANE_TABLE)),
        project_workflows,
        other_workflows,
        check_files,
        task_ids,
    }
}
