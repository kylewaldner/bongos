//! The names of the repository layout (everything under `definitions/`) and of the project
//! folder around the repository.

/// The one folder master holds: the top group's task folder in the nested task layout.
pub const DEFINITIONS_DIR: &str = "definitions";
/// `ProjectFile`, directly inside `definitions/`.
pub const PROJECT_FILE: &str = "project.toml";
/// A task's definition file; the top group's sits directly inside `definitions/`.
pub const TASK_FILE: &str = "task.toml";
/// The top group's plan, directly inside `definitions/`.
pub const PLAN_FILE: &str = "plan.md";
/// Documents of the project (under `definitions/`) or of a task (under its folder).
pub const DOCUMENTS_DIR: &str = "documents";
/// The folder holding a task's child tasks, one folder per task.
pub const TASKS_DIR: &str = "tasks";
/// Project workflows: `workflows/<dir>/workflow.toml` and the files it names.
pub const WORKFLOWS_DIR: &str = "workflows";
/// A workflow's definition file.
pub const WORKFLOW_FILE: &str = "workflow.toml";
/// Project profiles: `profiles/<name>/...`.
pub const PROFILES_DIR: &str = "profiles";

/// The folders `definitions/` holds before any is registered, besides the files named above.
pub const KNOWN_FOLDERS: [&str; 4] = [DOCUMENTS_DIR, TASKS_DIR, WORKFLOWS_DIR, PROFILES_DIR];

/// The only file committed outside `definitions/`, in the first commit.
pub const GITATTRIBUTES_FILE: &str = ".gitattributes";
/// Its contents: line endings never create textual conflicts across editors.
pub const GITATTRIBUTES_CONTENTS: &str = "* text=auto\n";

/// The project folder's non-bare clone, `main` checked out.
pub const REPOSITORY_DIR: &str = "repository";
/// The default place of working copies; never inside [`REPOSITORY_DIR`].
pub const WORKING_COPIES_DIR: &str = "working-copies";
/// The per-machine state of a project.
pub const STATE_DIR: &str = "state";
/// Files of tasks, one folder per task id (and one for the project id).
pub const FILES_DIR: &str = "files";

/// The state store, in [`STATE_DIR`].
pub const STATE_DB_FILE: &str = "state.db";
/// The host lock, in [`STATE_DIR`].
pub const HOST_LOCK_FILE: &str = "host.lock";
/// The landing lock, in [`STATE_DIR`].
pub const LANDING_LOCK_FILE: &str = "landing.lock";
/// Pinned commits, in [`STATE_DIR`].
pub const PINS_DIR: &str = "pins";
/// Replaced files, in [`STATE_DIR`].
pub const REPLACED_DIR: &str = "replaced";
/// The snapshot index, in [`STATE_DIR`].
pub const SNAPSHOT_INDEX_FILE: &str = "snapshot.index";

/// The only token of a layout template (the user setting `project_repository.layout`).
pub const FOLDER_TOKEN: &str = "{folder}";

/// The default template of each part, resolved once when a project is created.
pub const DEFAULT_REPOSITORY_TEMPLATE: &str = "{folder}/repository";
pub const DEFAULT_WORKING_COPIES_TEMPLATE: &str = "{folder}/working-copies";
pub const DEFAULT_STATE_TEMPLATE: &str = "{folder}/state";
pub const DEFAULT_FILES_TEMPLATE: &str = "{folder}/files";

/// `template` with [`FOLDER_TOKEN`] replaced by `folder`.
pub fn resolve_template(template: &str, folder: &str) -> String {
    template.replace(FOLDER_TOKEN, folder)
}

/// Whether `name` is one of [`KNOWN_FOLDERS`].
pub fn is_known_folder(name: &str) -> bool {
    KNOWN_FOLDERS.contains(&name)
}
