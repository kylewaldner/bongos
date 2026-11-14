//! The loader: a project's definitions read from a commit with one `git ls-tree -r` and one
//! `git cat-file --batch`, joined with a consistent read of its state into the work tree the
//! Work view serves. The checkout `repository/` is never read.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE, TASK_FILE};
use bongos_project_repo_model::{DefinitionTree, RelPath, work_tree_from};
use bongos_work_model::store::{Project, WorkTree};
use gpui::AppContext as _;
use parking_lot::Mutex;

use crate::create::{MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, Git, TreeRef};
use crate::state::{StateRead, StateReader, StateWriter};

/// A project's opened state store: the writer when this process holds it, else a reader.
#[derive(Clone)]
pub enum StateHandle {
    Writer(Arc<StateWriter>),
    Reader(Arc<StateReader>),
}

impl StateHandle {
    /// A consistent read.
    pub fn read<T>(&self, f: impl FnOnce(&StateRead) -> Result<T>) -> Result<T> {
        match self {
            StateHandle::Writer(writer) => writer.read(f),
            StateHandle::Reader(reader) => reader.read(f),
        }
    }

    pub fn writer(&self) -> Option<&Arc<StateWriter>> {
        match self {
            StateHandle::Writer(writer) => Some(writer),
            StateHandle::Reader(_) => None,
        }
    }
}

/// Every file path of the tree `treeish` (relative to the repository root), and the blobs of
/// `project.toml` and the `task.toml` files under `definitions/` with their paths relative to
/// it: one `git ls-tree -r` and one `git cat-file --batch`.
pub(crate) async fn definition_blobs(
    git: &Git,
    repository: &Path,
    treeish: &str,
) -> Result<(Vec<String>, Vec<(RelPath, Vec<u8>)>)> {
    let entries = git.ls_tree_r(repository, treeish).await?;
    let prefix = format!("{DEFINITIONS_DIR}/");
    let mut all = Vec::with_capacity(entries.len());
    let mut paths = Vec::new();
    let mut objects = Vec::new();
    for entry in entries {
        all.push(entry.path.clone());
        if entry.kind != "blob" {
            continue;
        }
        let Some(relative) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        let name = relative.rsplit('/').next().unwrap_or(relative);
        if name != TASK_FILE && name != PROJECT_FILE {
            continue;
        }
        let Some(path) = RelPath::new(relative) else {
            continue;
        };
        paths.push(path);
        objects.push(entry.object);
    }
    let blobs = git.cat_file_batch(repository, &objects).await?;
    let files = paths
        .into_iter()
        .zip(blobs)
        .filter_map(|(path, blob)| blob.map(|bytes| (path, bytes)))
        .collect();
    Ok((all, files))
}

/// Reads the definitions of the tree `at` in the repository at `repository`.
pub async fn load_definition_tree(
    git: &Git,
    repository: &Path,
    at: &TreeRef,
) -> Result<DefinitionTree> {
    let (_, files) = definition_blobs(git, repository, at.treeish()).await?;
    DefinitionTree::parse_files(files).map_err(|problems| {
        let list: Vec<String> = problems.iter().map(ToString::to_string).collect();
        anyhow!(
            "the definitions at {} cannot be read: {}",
            at.treeish(),
            list.join("; ")
        )
    })
}

impl ProjectRepo {
    fn state_or_error(&self) -> Result<&StateHandle> {
        self.state
            .as_ref()
            .context("the project's state store is not open")
    }

    /// The last commit that passed the deterministic checks, as recorded in the state store.
    pub fn verified_commit(&self) -> Result<Option<CommitId>> {
        Ok(self
            .state_or_error()?
            .read(|read| read.verified_commit())?
            .map(CommitId))
    }

    /// The commit the Work view shows: the verified commit, or before one is recorded the
    /// commit that created the project (made by bongos itself, it needs no checks). A commit
    /// made on `main` outside bongos before the first landing is therefore an outside commit.
    /// A project made before the creation commit was recorded falls back to the root of the
    /// first-parent history of `main`.
    pub async fn shown_commit(&self) -> Result<CommitId> {
        if let Some(commit) = self.verified_commit()? {
            return Ok(commit);
        }
        if let Some(commit) = self
            .state_or_error()?
            .read(|read| read.meta(crate::create::CREATION_COMMIT_KEY))?
        {
            return Ok(CommitId(commit));
        }
        let roots = self
            .git
            .run(
                &self.location.repository,
                &["rev-list", "--first-parent", "--max-parents=0", MAIN_BRANCH],
            )
            .await?;
        let commit = roots
            .lines()
            .last()
            .context("master has no first commit")?
            .to_string();
        Ok(CommitId(commit))
    }

    /// The definitions of the tree `at`.
    pub fn definition_tree(
        &self,
        at: TreeRef,
        cx: &gpui::App,
    ) -> gpui::Task<Result<DefinitionTree>> {
        let git = self.git.clone();
        let repository = self.location.repository.clone();
        cx.background_spawn(async move { load_definition_tree(&git, &repository, &at).await })
    }

    /// The work tree of the tree `at` joined with the current state, read consistently.
    pub fn work_tree_at(&self, at: TreeRef, cx: &gpui::App) -> gpui::Task<Result<WorkTree>> {
        let repo = self.clone();
        cx.background_spawn(async move { repo.load_work_tree_at(&at).await })
    }

    /// [`Self::work_tree_at`] without an app.
    pub async fn load_work_tree_at(&self, at: &TreeRef) -> Result<WorkTree> {
        let definitions = load_definition_tree(&self.git, &self.location.repository, at).await?;
        self.join_with_state(&definitions)
    }

    /// `definitions` joined with one consistent read of the state, with the pending mark of
    /// every task an open quick edit changes.
    pub fn join_with_state(&self, definitions: &DefinitionTree) -> Result<WorkTree> {
        let files = self.location.files.clone();
        self.state_or_error()?.read(|read| {
            let mut tree = work_tree_from(definitions, read, &files);
            crate::quick_edit::mark_pending(&mut tree, read)?;
            Ok(tree)
        })
    }
}

/// An open project as the Work view serves it: the definitions of the commit it shows, kept
/// parsed, joined with the state on every read.
pub struct LoadedProject {
    repo: ProjectRepo,
    shown: Mutex<(Option<CommitId>, Arc<DefinitionTree>)>,
}

impl LoadedProject {
    /// `repo` must have its state store open.
    pub fn new(repo: ProjectRepo) -> Arc<Self> {
        Arc::new(Self {
            repo,
            shown: Mutex::new((None, Arc::new(DefinitionTree::default()))),
        })
    }

    pub fn repo(&self) -> &ProjectRepo {
        &self.repo
    }

    /// The commit whose definitions are loaded, `None` before the first load.
    pub fn commit(&self) -> Option<CommitId> {
        self.shown.lock().0.clone()
    }

    pub fn definitions(&self) -> Arc<DefinitionTree> {
        self.shown.lock().1.clone()
    }

    /// The loaded commit and its definitions, read together.
    pub fn shown(&self) -> (Option<CommitId>, Arc<DefinitionTree>) {
        self.shown.lock().clone()
    }

    /// Loads the definitions of the commit to show when it differs from the loaded one, and
    /// hands them to the writer for its hooks. Returns whether they changed.
    pub async fn load(&self) -> Result<bool> {
        let commit = self.repo.shown_commit().await?;
        if self.commit().as_ref() == Some(&commit) {
            return Ok(false);
        }
        let definitions = Arc::new(
            load_definition_tree(
                &self.repo.git,
                &self.repo.location.repository,
                &TreeRef::Commit(commit.clone()),
            )
            .await?,
        );
        if let Some(writer) = self.repo.state.as_ref().and_then(StateHandle::writer) {
            writer.set_definitions_at(&commit.0, definitions.clone());
        }
        *self.shown.lock() = (Some(commit), definitions);
        Ok(true)
    }

    /// The loaded definitions joined with the current state.
    pub fn work_tree(&self) -> Result<WorkTree> {
        self.repo.join_with_state(&self.definitions())
    }

    /// The project as the Work view lists it; none before the first load.
    pub fn projects(&self) -> Result<Vec<Project>> {
        Ok(self.work_tree()?.projects)
    }
}
