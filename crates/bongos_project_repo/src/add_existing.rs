//! `project: add existing`: adds a project repository that already exists, as a folder on this
//! machine or at a remote address cloned into a new project folder. It fetches master and the
//! state branch, checks that `definitions/project.toml` parses, makes the missing parts of the
//! project folder, runs the deterministic checks on master to set the verified commit, writes
//! the location record last, and says whether a snapshot is there to restore.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_project_repo_model::layout;
use bongos_work_model::parse_project;

use crate::backup::{PushTimeout, real_push_timeout, remote_problem, shown_remote};
use crate::create::{LayoutTemplates, MAIN_BRANCH, ProjectRepo};
use crate::git::{CommitId, Git, GitError, TreeRef};
use crate::landing_checks::CheckRegistry;
use crate::loader::{StateHandle, load_definition_tree};
use crate::locations::{Location, RepositoryLocations};
use crate::restore::REMOTE_STATE_REF;
use crate::snapshot::Secrets;
use crate::state::StateStore;
use crate::state_branch;
use crate::state_hooks::StateApplyHooks;

/// Where the project to add is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddExistingSource {
    /// A project folder (its repository where the layout puts it), or a repository folder
    /// itself, whose parent becomes the project folder.
    Path { path: PathBuf },
    /// A remote address, cloned into `folder`, which must not exist.
    Remote { address: String, folder: PathBuf },
}

/// What `project: add existing` is given.
#[derive(Clone)]
pub struct AddExistingRequest {
    pub source: AddExistingSource,
    pub layout: LayoutTemplates,
    /// The state branch, without `refs/heads/`.
    pub state_branch: String,
    /// For a folder: the backup remote, whose state branch is fetched. A clone's is its
    /// address.
    pub remote: Option<String>,
    /// The checks registered in the app; the built-in ones when `None`.
    pub checks: Option<CheckRegistry>,
    /// Gives up a clone or fetch when it resolves; the push timeout when `None`.
    pub timeout: Option<PushTimeout>,
    /// The patterns withheld from the text of a failed clone or fetch.
    pub secret_patterns: Vec<String>,
}

/// A project added.
#[derive(Debug)]
pub struct AddedProject {
    pub repo: ProjectRepo,
    /// The verified commit: master, when it passed the deterministic checks.
    pub verified: Option<String>,
    /// What the checks found wrong on master; the project shows it once it passes.
    pub problems: Vec<String>,
    /// A snapshot is there to restore, here or fetched from the remote.
    pub has_snapshot: bool,
    /// What could not be fetched.
    pub notes: Vec<String>,
}

/// What adding changed so far, undone when adding fails: the directories and files it made,
/// and the files it made read-only, with the permissions they had.
#[derive(Default)]
struct Made {
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
    protected: Vec<(PathBuf, std::fs::Permissions)>,
}

impl Made {
    fn dir(&mut self, path: &Path) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        let top = path
            .ancestors()
            .take_while(|ancestor| !ancestor.exists())
            .last()
            .unwrap_or(path)
            .to_path_buf();
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        self.dirs.push(top);
        Ok(())
    }

    fn undo(&self) {
        for (path, permissions) in &self.protected {
            std::fs::set_permissions(path, permissions.clone()).ok();
        }
        for path in &self.files {
            std::fs::remove_file(path).ok();
        }
        for path in self.dirs.iter().rev() {
            std::fs::remove_dir_all(path).ok();
        }
    }
}

/// `path` with its links and `..` resolved, or as given when it cannot be.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve(template: &str, folder: &Path) -> PathBuf {
    PathBuf::from(layout::resolve_template(
        template,
        &folder.to_string_lossy(),
    ))
}

fn is_repository(path: &Path) -> bool {
    path.join(".git").exists()
}

/// `message` with the credentials of `remote` and every match of `secret_patterns` withheld.
fn cleaned(message: &str, remote: &str, secret_patterns: &[String]) -> String {
    let message = message.replace(remote, &shown_remote(remote));
    match Secrets::new(secret_patterns, &[]) {
        Ok(secrets) => secrets.clean(&message),
        Err(_) => "git failed".to_string(),
    }
}

impl ProjectRepo {
    /// Adds the project `request` names, recording its location in `locations`. A failure
    /// removes every folder it made and records nothing.
    pub async fn add_existing(
        request: AddExistingRequest,
        locations: &mut RepositoryLocations,
        find_git: impl FnOnce() -> Result<Git, GitError>,
    ) -> Result<AddedProject> {
        let git = find_git()?;
        let mut made = Made::default();
        match Self::add(&git, request, locations, &mut made).await {
            Ok(added) => Ok(added),
            Err(error) => {
                made.undo();
                Err(error)
            }
        }
    }

    async fn add(
        git: &Git,
        request: AddExistingRequest,
        locations: &mut RepositoryLocations,
        made: &mut Made,
    ) -> Result<AddedProject> {
        let templates = &request.layout;
        let reference = state_branch::state_ref(&request.state_branch)?;
        let timeout = || -> futures::future::BoxFuture<'static, ()> {
            match &request.timeout {
                Some(timeout) => timeout(),
                None => real_push_timeout(),
            }
        };
        let mut notes = Vec::new();
        let (folder, repository, remote) = match &request.source {
            AddExistingSource::Remote { address, folder } => {
                if let Some(problem) = remote_problem(address) {
                    bail!("the address {problem}");
                }
                if folder.exists() {
                    bail!("{} already exists; pick a new folder", folder.display());
                }
                made.dir(folder)?;
                let repository = resolve(&templates.repository, folder);
                if let Some(parent) = repository.parent() {
                    made.dir(parent)?;
                }
                git.clone_repo(address, &repository, MAIN_BRANCH, timeout())
                    .await
                    .map_err(|error| {
                        anyhow!(
                            "{}",
                            cleaned(
                                &format!("cloning {address}: {error}"),
                                address,
                                &request.secret_patterns
                            )
                        )
                    })?;
                // The clone brought every branch of the remote; the state branch becomes this
                // machine's.
                let fetched = format!("refs/remotes/origin/{}", request.state_branch);
                if let Ok(commit) = git.rev_parse(&repository, &fetched).await {
                    git.update_ref(&repository, &reference, &commit).await?;
                }
                (folder.clone(), repository, Some(address.clone()))
            }
            AddExistingSource::Path { path } => {
                if !path.is_dir() {
                    bail!("{} is not a folder", path.display());
                }
                let inside = resolve(&templates.repository, path);
                let (folder, repository) = if is_repository(&inside) {
                    (path.clone(), inside)
                } else if is_repository(path) {
                    let folder = path
                        .parent()
                        .with_context(|| format!("{} has no parent folder", path.display()))?
                        .to_path_buf();
                    (folder, path.clone())
                } else {
                    bail!(
                        "{} is neither a project folder nor a project repository",
                        path.display()
                    );
                };
                if let Some(remote) = &request.remote {
                    if let Some(problem) = remote_problem(remote) {
                        bail!("the remote {problem}");
                    }
                    let refspec = format!("+{reference}:{REMOTE_STATE_REF}");
                    if let Err(error) = git
                        .fetch_refspecs(&repository, remote, &[refspec], timeout())
                        .await
                    {
                        notes.push(format!(
                            "the remote's state branch could not be fetched: {}",
                            cleaned(&error.to_string(), remote, &request.secret_patterns)
                        ));
                    }
                }
                (folder, repository, request.remote.clone())
            }
        };

        let main = git
            .rev_parse(&repository, MAIN_BRANCH)
            .await
            .map_err(|_| anyhow!("the repository has no {MAIN_BRANCH} branch"))?;
        let project_path = format!(
            "{main}:{}/{}",
            layout::DEFINITIONS_DIR,
            layout::PROJECT_FILE
        );
        let project_text = git
            .cat_file_batch(&repository, &[project_path])
            .await?
            .into_iter()
            .next()
            .flatten()
            .with_context(|| {
                format!(
                    "{MAIN_BRANCH} has no {}/{}; this is not a project repository",
                    layout::DEFINITIONS_DIR,
                    layout::PROJECT_FILE
                )
            })?;
        let project_text = String::from_utf8(project_text).map_err(|_| {
            anyhow!(
                "{}/{} is not text",
                layout::DEFINITIONS_DIR,
                layout::PROJECT_FILE
            )
        })?;
        let project = parse_project(&project_text).map_err(|error| {
            anyhow!(
                "{}/{} does not parse: {error}",
                layout::DEFINITIONS_DIR,
                layout::PROJECT_FILE
            )
        })?;
        let id = project
            .id
            .filter(|id| !id.trim().is_empty())
            .with_context(|| {
                format!(
                    "{}/{} has no id",
                    layout::DEFINITIONS_DIR,
                    layout::PROJECT_FILE
                )
            })?;
        if let Some(known) = locations.location(&id)
            && canonical(&known.repository) != canonical(&repository)
        {
            bail!(
                "this project is already on this machine, at {}",
                known.folder.display()
            );
        }

        let location = Location {
            id,
            working_copies: resolve(&templates.working_copies, &folder),
            state: resolve(&templates.state, &folder),
            files: resolve(&templates.files, &folder),
            folder,
            repository,
            remote,
            converted_from: None,
        };
        for path in [&location.working_copies, &location.state, &location.files] {
            made.dir(path)?;
        }
        let database = location.state.join(layout::STATE_DB_FILE);
        if std::fs::symlink_metadata(&database).is_err() {
            // The store and its WAL files are this call's: a failure removes them.
            for suffix in ["", "-wal", "-shm"] {
                let mut name = database.clone().into_os_string();
                name.push(suffix);
                made.files.push(PathBuf::from(name));
            }
        }
        let host_lock = location.state.join(layout::HOST_LOCK_FILE);
        if std::fs::symlink_metadata(&host_lock).is_err() {
            made.files.push(host_lock);
        }
        // The hooks of the app are not run: nothing of this project is loaded yet.
        let writer = Arc::new(
            StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new())
                .map_err(|error| anyhow!("{error}"))?,
        );
        let repo = ProjectRepo::new(location.clone(), git.clone())
            .with_state(StateHandle::Writer(writer.clone()));
        if let Some(checks) = request.checks {
            *repo.engine.registered.lock() = checks;
        }
        let problems: Vec<String> = repo
            .schema_problems_at(&main)
            .await?
            .iter()
            .map(ToString::to_string)
            .collect();
        let verified = if problems.is_empty() {
            let definitions = Arc::new(
                load_definition_tree(
                    git,
                    &location.repository,
                    &TreeRef::Commit(CommitId(main.clone())),
                )
                .await?,
            );
            writer.transact(|tx| tx.set_verified_commit(&main, definitions.clone()))?;
            Some(main)
        } else {
            None
        };
        crate::working_copies::protect_files_noting(&location.repository, &mut made.protected)
            .context("making master read-only")?;
        let has_snapshot = state_branch::tip(git, &location.repository, &reference)
            .await
            .is_some()
            || state_branch::tip(git, &location.repository, REMOTE_STATE_REF)
                .await
                .is_some();
        locations.record(location)?;
        Ok(AddedProject {
            repo,
            verified,
            problems,
            has_snapshot,
            notes,
        })
    }
}
