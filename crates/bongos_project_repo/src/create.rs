//! Creating a project repository: the project folder with its repository, working copies,
//! state and files parts.

use std::path::{Path, PathBuf};

use bongos_project_repo_model::{layout, split};
use bongos_work_model::ProjectFile;
use bongos_work_model::{Kind, TaskFile, new_id, serialize_project};

use crate::git::{Git, GitError, Identity};
use crate::loader::StateHandle;
use crate::locations::{Location, LocationError, RepositoryLocations};

/// The branch master is.
pub const MAIN_BRANCH: &str = "main";
const FIRST_COMMIT_MESSAGE: &str = "create the project";
/// The `meta` key of the commit that created the project.
pub(crate) const CREATION_COMMIT_KEY: &str = "creation_commit";

/// The template of each part of a project folder; `{folder}` is the only token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutTemplates {
    pub repository: String,
    pub working_copies: String,
    pub state: String,
    pub files: String,
}

impl Default for LayoutTemplates {
    fn default() -> Self {
        Self {
            repository: layout::DEFAULT_REPOSITORY_TEMPLATE.to_string(),
            working_copies: layout::DEFAULT_WORKING_COPIES_TEMPLATE.to_string(),
            state: layout::DEFAULT_STATE_TEMPLATE.to_string(),
            files: layout::DEFAULT_FILES_TEMPLATE.to_string(),
        }
    }
}

impl LayoutTemplates {
    fn resolve(&self, template: &str, folder: &Path) -> PathBuf {
        PathBuf::from(layout::resolve_template(
            template,
            &folder.to_string_lossy(),
        ))
    }
}

/// What a new project repository is made from.
#[derive(Clone, Debug)]
pub struct CreateRequest {
    pub title: String,
    pub folder: PathBuf,
    pub layout: LayoutTemplates,
    /// Names of code repositories from the settings.
    pub repositories: Vec<String>,
    pub remote: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Location(#[from] LocationError),
    #[error("a project needs a title")]
    NoTitle,
    #[error("{0} already exists")]
    Exists(PathBuf),
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
}

/// A project repository on this machine: where its parts are, the git that reads it, and its
/// state store once opened.
#[derive(Clone)]
pub struct ProjectRepo {
    pub(crate) location: Location,
    pub(crate) git: Git,
    pub(crate) state: Option<StateHandle>,
    /// The landing queue and its waiters, shared by every clone.
    pub(crate) engine: std::sync::Arc<crate::landing_queue::Engine>,
}

impl std::fmt::Debug for ProjectRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectRepo")
            .field("location", &self.location)
            .field("git", &self.git)
            .field("state_open", &self.state.is_some())
            .finish()
    }
}

impl ProjectRepo {
    /// The project at `location`, read with `git`, its state store not opened.
    pub fn new(location: Location, git: Git) -> Self {
        Self {
            location,
            git,
            state: None,
            engine: Default::default(),
        }
    }

    /// The same project reading and writing its state through `state`.
    pub fn with_state(mut self, state: StateHandle) -> Self {
        self.state = Some(state);
        self
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// The opened state store, if any.
    pub fn state(&self) -> Option<&StateHandle> {
        self.state.as_ref()
    }

    /// Creates the project folder, runs `git init`, commits the definitions on `main`, makes the
    /// empty state area and records the location.
    pub async fn create(
        request: CreateRequest,
        locations: &mut RepositoryLocations,
    ) -> Result<Self, CreateError> {
        Self::create_with(request, locations, Git::locate).await
    }

    /// [`Self::create`] with the lookup of git supplied. Git is looked up before anything is
    /// created, so a machine without it ends here with nothing made.
    pub async fn create_with(
        request: CreateRequest,
        locations: &mut RepositoryLocations,
        find_git: impl FnOnce() -> Result<Git, GitError>,
    ) -> Result<Self, CreateError> {
        let git = find_git()?;
        let title = request.title.trim().to_string();
        if title.is_empty() {
            return Err(CreateError::NoTitle);
        }
        let folder = request.folder.clone();
        if folder.exists() {
            return Err(CreateError::Exists(folder));
        }
        let mut made = Vec::new();
        match Self::build(&git, request, title, locations, &mut made).await {
            Ok(repo) => Ok(repo),
            Err(error) => {
                for path in made.iter().rev() {
                    std::fs::remove_dir_all(path).ok();
                }
                Err(error)
            }
        }
    }

    async fn build(
        git: &Git,
        request: CreateRequest,
        title: String,
        locations: &mut RepositoryLocations,
        made: &mut Vec<PathBuf>,
    ) -> Result<Self, CreateError> {
        let folder = request.folder;
        let templates = &request.layout;
        let repository = templates.resolve(&templates.repository, &folder);
        let working_copies = templates.resolve(&templates.working_copies, &folder);
        let state = templates.resolve(&templates.state, &folder);
        let files = templates.resolve(&templates.files, &folder);
        for path in [&folder, &repository, &working_copies, &state, &files] {
            make_dir(path, made)?;
        }

        let project_id = new_id();
        let mut project = ProjectFile::new(title.clone());
        project.id = Some(project_id.clone());
        project.repositories = request.repositories;
        let project_text = serialize_project(&project).map_err(|error| CreateError::Io {
            context: "writing project.toml".to_string(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let (definition, _) = split(&TaskFile::new(title, Kind::Other));
        let task_text = toml::to_string(&definition).map_err(|error| CreateError::Io {
            context: "writing task.toml".to_string(),
            source: std::io::Error::other(error.to_string()),
        })?;

        let definitions = repository.join(layout::DEFINITIONS_DIR);
        make_dir(&definitions, made)?;
        write(
            &repository.join(layout::GITATTRIBUTES_FILE),
            layout::GITATTRIBUTES_CONTENTS,
        )?;
        write(&definitions.join(layout::PROJECT_FILE), &project_text)?;
        write(&definitions.join(layout::TASK_FILE), &task_text)?;
        write(&definitions.join(layout::PLAN_FILE), "")?;

        git.init_repo(&repository, MAIN_BRANCH).await?;
        let first = git
            .commit_all(&repository, FIRST_COMMIT_MESSAGE, &Identity::default())
            .await?;
        crate::working_copies::protect_files(&repository).map_err(|source| CreateError::Io {
            context: "making master read-only".to_string(),
            source,
        })?;

        let location = Location {
            id: project_id,
            folder,
            repository,
            working_copies,
            state,
            files,
            remote: request.remote,
            converted_from: None,
        };
        record_creation_commit(&location, &first)?;
        locations.record(location.clone())?;
        Ok(Self::new(location, git.clone()))
    }
}

/// Records `commit` in the new state store as the commit that created the project: what master
/// is before the first landing.
fn record_creation_commit(location: &Location, commit: &str) -> Result<(), CreateError> {
    let failed = |error: String| CreateError::Io {
        context: "recording the first commit in the state store".to_string(),
        source: std::io::Error::other(error),
    };
    let writer =
        crate::StateStore::open_writer_with_hooks(location, &crate::StateApplyHooks::new())
            .map_err(|error| failed(error.to_string()))?;
    writer
        .transact(|tx| tx.set_meta(CREATION_COMMIT_KEY, commit))
        .map_err(|error| failed(format!("{error:#}")))
}

/// Creates `path` and the missing directories above it, noting the topmost one made so a
/// failure can remove it.
fn make_dir(path: &Path, made: &mut Vec<PathBuf>) -> Result<(), CreateError> {
    if path.exists() {
        return Ok(());
    }
    let top = path
        .ancestors()
        .take_while(|ancestor| !ancestor.exists())
        .last()
        .unwrap_or(path)
        .to_path_buf();
    std::fs::create_dir_all(path).map_err(|source| CreateError::Io {
        context: format!("creating {}", path.display()),
        source,
    })?;
    made.push(top);
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), CreateError> {
    std::fs::write(path, text).map_err(|source| CreateError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}
