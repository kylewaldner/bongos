//! The facts of a change that the checks read (both trees' files and parsed definitions, the
//! changed files with their contents, the change summary), and the built-in in-queue checks
//! 1 to 4 and 6 to 8 run over them. The checks themselves are pure and live in
//! `bongos_project_repo_model::checks`; this reads git once per tree and feeds them.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use bongos_project_repo_model::checks::{
    CheckOutcome, FileFacts, LiveFacts, NO_CYCLE_CHECK, NO_STATE_CHECK, REFERENCES_CHECK,
    SCHEMA_CHECK, SECRETS_CHECK, SIZE_AND_KIND_CHECK, STARTED_UNCHANGED_CHECK, SizeLimits,
    StagedTodoMove, Stamp, StartedInput, compile_secret_patterns, landing_declaration_problems,
    no_cycle, no_state, references, schema_problems, secrets, size_and_kind, started_unchanged,
};
use bongos_project_repo_model::layout::{
    DEFINITIONS_DIR, PROJECT_FILE, TASK_FILE, WORKFLOW_FILE, WORKFLOWS_DIR,
};
use bongos_project_repo_model::{
    Author, ChangeMeta, ChangeSummary, DefinitionTree, FileChange, Problem, RelPath,
    TaskDefinition, summarize,
};

use crate::git::{Git, TreeEntry};
use crate::landing_checks::CheckRegistry;
use crate::state_ops::StateOp;

/// One tree as the checks read it.
pub(crate) struct TreeFacts {
    pub(crate) id: String,
    pub(crate) entries: Vec<TreeEntry>,
    /// `project.toml` and the `task.toml` files, relative to `definitions/`.
    pub(crate) files: Vec<(RelPath, Vec<u8>)>,
    /// The files of the registered definition folders, relative to `definitions/`.
    pub(crate) folder_files: Vec<(String, Vec<u8>)>,
    pub(crate) definitions: Result<DefinitionTree, Vec<Problem>>,
}

impl TreeFacts {
    pub(crate) fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.path.as_str())
    }

    /// The parsed definitions, or empty ones when they do not parse.
    pub(crate) fn definitions_or_empty(&self) -> DefinitionTree {
        self.definitions.clone().unwrap_or_default()
    }
}

/// Reads the tree `treeish` with one `git ls-tree -r` and one `git cat-file --batch`.
pub(crate) async fn tree_facts(
    git: &Git,
    repository: &Path,
    treeish: &str,
    folders: &[String],
) -> Result<TreeFacts> {
    let id = git.tree_of(repository, treeish).await?;
    let entries = git.ls_tree_r(repository, &id).await?;
    let prefix = format!("{DEFINITIONS_DIR}/");
    let mut wanted: Vec<(String, bool)> = Vec::new();
    let mut objects = Vec::new();
    for entry in &entries {
        if entry.kind != "blob" {
            continue;
        }
        let Some(relative) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        let name = relative.rsplit('/').next().unwrap_or(relative);
        let in_folder = relative
            .split_once('/')
            .is_some_and(|(folder, _)| folders.iter().any(|known| known == folder));
        if in_folder {
            wanted.push((relative.to_string(), true));
        } else if name == TASK_FILE || name == PROJECT_FILE {
            wanted.push((relative.to_string(), false));
        } else {
            continue;
        }
        objects.push(entry.object.clone());
    }
    let blobs = git.cat_file_batch(repository, &objects).await?;
    let mut files = Vec::new();
    let mut folder_files = Vec::new();
    for ((relative, in_folder), blob) in wanted.into_iter().zip(blobs) {
        let Some(bytes) = blob else {
            continue;
        };
        if in_folder {
            folder_files.push((relative, bytes));
        } else if let Some(path) = RelPath::new(relative) {
            files.push((path, bytes));
        }
    }
    let definitions = DefinitionTree::parse_files(files.clone());
    Ok(TreeFacts {
        id,
        entries,
        files,
        folder_files,
        definitions,
    })
}

/// A file that differs between master and the change, with its contents in the change.
pub(crate) struct ChangedFile {
    /// From the repository root.
    pub(crate) path: String,
    pub(crate) change: FileChange,
    pub(crate) bytes: Option<Vec<u8>>,
}

/// What the checks of one change on one tree read.
pub(crate) struct ChangeFacts {
    pub(crate) master_commit: String,
    pub(crate) master: TreeFacts,
    pub(crate) new: TreeFacts,
    pub(crate) changed: Vec<ChangedFile>,
    pub(crate) summary: ChangeSummary,
}

impl ChangeFacts {
    /// Reads master at `master_commit` and the change at `treeish`.
    pub(crate) async fn read(
        git: &Git,
        repository: &Path,
        master_commit: &str,
        treeish: &str,
        folders: &[String],
        author: &Author,
    ) -> Result<Self> {
        let master = tree_facts(git, repository, master_commit, folders).await?;
        let new = tree_facts(git, repository, treeish, folders).await?;
        let before: BTreeMap<&str, &str> = master
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.object.as_str()))
            .collect();
        let mut changed: Vec<ChangedFile> = Vec::new();
        let mut objects = Vec::new();
        for entry in &new.entries {
            let change = match before.get(entry.path.as_str()) {
                None => FileChange::Added,
                Some(object) if *object != entry.object => FileChange::Modified,
                Some(_) => continue,
            };
            changed.push(ChangedFile {
                path: entry.path.clone(),
                change,
                bytes: None,
            });
            objects.push(entry.object.clone());
        }
        let blobs = git.cat_file_batch(repository, &objects).await?;
        for (file, blob) in changed.iter_mut().zip(blobs) {
            file.bytes = blob;
        }
        let now: std::collections::BTreeSet<&str> = new
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        for entry in &master.entries {
            if !now.contains(entry.path.as_str()) {
                changed.push(ChangedFile {
                    path: entry.path.clone(),
                    change: FileChange::Removed,
                    bytes: None,
                });
            }
        }
        changed.sort_by(|a, b| a.path.cmp(&b.path));
        let prefix = format!("{DEFINITIONS_DIR}/");
        let files = changed
            .iter()
            .map(|file| {
                let path = file.path.strip_prefix(&prefix).unwrap_or(&file.path);
                (path.to_string(), file.change)
            })
            .collect();
        let summary = summarize(&master.definitions_or_empty(), &new.definitions_or_empty())
            .with_files(files)
            .with_author(author.clone());
        Ok(Self {
            master_commit: master_commit.to_string(),
            master,
            new,
            changed,
            summary,
        })
    }

    /// The ids of the tasks the change adds, removes, moves or redefines; every task id of
    /// either tree when one of them does not parse.
    pub(crate) fn touched_task_ids(&self) -> Vec<String> {
        let (Ok(master), Ok(new)) = (&self.master.definitions, &self.new.definitions) else {
            return self.task_ids();
        };
        let shape = |tree: &DefinitionTree| -> BTreeMap<String, (String, TaskDefinition)> {
            tree.nodes()
                .into_iter()
                .map(|node| {
                    (
                        node.definition.id.clone(),
                        (node.folder.clone(), node.definition.clone()),
                    )
                })
                .collect()
        };
        let (before, after) = (shape(master), shape(new));
        let mut ids: Vec<String> = before
            .keys()
            .chain(after.keys())
            .filter(|id| before.get(*id) != after.get(*id))
            .cloned()
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Every task id of either tree.
    pub(crate) fn task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = [&self.master, &self.new]
            .into_iter()
            .filter_map(|tree| tree.definitions.as_ref().ok())
            .flat_map(|tree| {
                tree.nodes()
                    .into_iter()
                    .map(|node| node.definition.id.clone())
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// The staged `MoveTodo` ops of a change, as check 6 reads them.
pub(crate) fn staged_moves(ops: &[StateOp]) -> Vec<StagedTodoMove> {
    ops.iter()
        .filter_map(|op| match op {
            StateOp::MoveTodo {
                from_task,
                to_task,
                todo_id,
            } => Some(StagedTodoMove {
                from_task: from_task.clone(),
                to_task: to_task.clone(),
                todo_id: todo_id.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// What the built-ins read besides the facts.
pub(crate) struct BuiltinInput<'a> {
    pub(crate) facts: &'a ChangeFacts,
    pub(crate) registry: &'a CheckRegistry,
    pub(crate) live: &'a LiveFacts,
    pub(crate) change_id: &'a str,
    pub(crate) meta: &'a ChangeMeta,
    pub(crate) staged_moves: &'a [StagedTodoMove],
    pub(crate) size_limits: SizeLimits,
    pub(crate) secret_patterns: &'a [String],
}

/// The paths under `definitions/` check 1 allows besides the known ones: the stamp files of
/// the amending checks and the registered folders.
pub(crate) fn allowed_extra(registry: &CheckRegistry) -> Vec<String> {
    registry
        .amending
        .iter()
        .flat_map(|check| check.stamp_paths())
        .map(|path| path.as_str().to_string())
        .chain(registry.folders.iter().map(|(name, _)| format!("{name}/")))
        .collect()
}

/// Check 1 over a tree: its layout and definitions, the project's `[[landing.checks]]`
/// entries, and the files of the registered folders.
pub(crate) fn schema_check(tree: &TreeFacts, registry: &CheckRegistry) -> Vec<Problem> {
    let mut problems = schema_problems(tree.paths(), tree.files.clone(), &allowed_extra(registry));
    if let Ok(definitions) = &tree.definitions
        && let Some(project) = &definitions.project
    {
        let amending: Vec<&str> = registry.amending.iter().map(|check| check.name()).collect();
        problems.extend(landing_declaration_problems(project, &amending));
        problems.extend(crate::declared_checks::entry_problems(
            Some(project),
            registry,
        ));
    }
    for (path, bytes) in &tree.folder_files {
        let Some((folder, _)) = path.split_once('/') else {
            continue;
        };
        for (name, validator) in &registry.folders {
            if name == folder {
                let text = String::from_utf8_lossy(bytes);
                problems.extend(validator(Path::new(path), &text));
            }
        }
    }
    problems
}

const UNPARSED: &str = "the definitions do not parse (see the schema check)";

/// Runs the built-in `name`.
pub(crate) fn run_builtin(name: &str, input: &BuiltinInput) -> CheckOutcome {
    let facts = input.facts;
    let parsed = facts.new.definitions.as_ref();
    match name {
        SCHEMA_CHECK => {
            let problems = schema_check(&facts.new, input.registry);
            let list: Vec<String> = problems.iter().map(ToString::to_string).collect();
            CheckOutcome::from_refusals(if list.is_empty() {
                Vec::new()
            } else {
                vec![list.join("; ")]
            })
        }
        NO_STATE_CHECK => {
            let texts: Vec<(String, String)> = facts
                .new
                .files
                .iter()
                .filter(|(path, _)| path.file_name() == TASK_FILE)
                .map(|(path, bytes)| {
                    (
                        format!("{DEFINITIONS_DIR}/{}", path.as_str()),
                        String::from_utf8_lossy(bytes).into_owned(),
                    )
                })
                .collect();
            no_state(
                facts.new.paths(),
                texts
                    .iter()
                    .map(|(path, text)| (path.as_str(), text.as_str())),
            )
        }
        NO_CYCLE_CHECK => match parsed {
            Ok(tree) => no_cycle(tree, &facts.summary),
            Err(_) => CheckOutcome::no_verdict(UNPARSED),
        },
        REFERENCES_CHECK => match parsed {
            Ok(tree) => {
                let prefix = format!("{DEFINITIONS_DIR}/{WORKFLOWS_DIR}/");
                let in_project = |workflow: &str| {
                    let file = format!("{prefix}{workflow}/{WORKFLOW_FILE}");
                    facts.new.paths().any(|path| path == file)
                };
                let resolves = |workflow: &str| {
                    in_project(workflow)
                        || input
                            .registry
                            .workflows
                            .as_ref()
                            .is_none_or(|resolver| resolver(workflow))
                };
                references(tree, &resolves)
            }
            Err(_) => CheckOutcome::no_verdict(UNPARSED),
        },
        STARTED_UNCHANGED_CHECK => match (&facts.master.definitions, parsed) {
            (Ok(master), Ok(new)) => started_unchanged(&StartedInput {
                master,
                new,
                summary: &facts.summary,
                live: input.live,
                change_id: input.change_id,
                meta: input.meta,
                staged_moves: input.staged_moves,
            }),
            _ => CheckOutcome::no_verdict(UNPARSED),
        },
        SIZE_AND_KIND_CHECK => size_and_kind(
            facts.changed.iter().filter_map(|file| {
                let bytes = file.bytes.as_deref()?;
                Some(FileFacts {
                    path: &file.path,
                    len: bytes.len() as u64,
                    head: bytes,
                })
            }),
            input.size_limits,
        ),
        SECRETS_CHECK => match compile_secret_patterns(input.secret_patterns) {
            Ok(patterns) => secrets(
                facts.changed.iter().filter_map(|file| {
                    let text = std::str::from_utf8(file.bytes.as_deref()?).ok()?;
                    Some((file.path.as_str(), text))
                }),
                &patterns,
                // The schema marks no key secret yet; the patterns are the whole check for now.
                &[],
            ),
            Err(error) => CheckOutcome::no_verdict(error),
        },
        other => CheckOutcome::no_verdict(format!("{other} is not a built-in check")),
    }
}

/// The stamp of a built-in's result: the tree, and for check 6 also the started tasks it saw,
/// so a task started since reruns it: a result judged against an older started set is stale.
pub(crate) fn builtin_stamp(name: &str, input: &BuiltinInput) -> Stamp {
    if name != STARTED_UNCHANGED_CHECK {
        return Stamp::tree(input.facts.new.id.clone());
    }
    let ids = input.facts.task_ids();
    let started = input.live.started_set(ids.iter().map(String::as_str));
    let started: Vec<&str> = started.iter().map(String::as_str).collect();
    Stamp(format!(
        "{} started: {}",
        input.facts.new.id,
        started.join(",")
    ))
}
