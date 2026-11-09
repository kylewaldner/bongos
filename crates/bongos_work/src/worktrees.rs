//! `task: start` (D14): the branch and git worktree a leaf task's work happens in, created under
//! `<repository>/.bongos-worktrees/`, a directory every worktree of the repository ignores
//! through the common git dir's `info/exclude` (D15). Nothing existing is ever overwritten: a
//! path or branch that is already taken refuses with a toast, and a task gets at most one
//! worktree.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_work_model::commands::{self as model, StartedWorktree};
use bongos_work_model::derive::{self, Shape, State};
use bongos_work_model::files::write_atomic;
use bongos_work_model::rules::{Place, linear_branch_segment};
use bongos_work_model::slug::oneoff_slug;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Kind, ResultKind, Status, TaskFile};
use fs::Fs;
use git::repository::{CreateWorktreeTarget, GitRepository, Worktree};
use gpui::{AppContext as _, AsyncWindowContext, Context, SharedString, WeakEntity, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::actions::task;
use crate::commands::{is_open_leaf, queue, ready, show_toast, with_task};
use crate::repositories::resolve_repository;
use crate::settings::Repository;
use crate::{WorkSettings, WorkStore, modals};

/// The directory under a repository's root that holds the worktrees of its tasks.
pub const WORKTREES_DIR: &str = ".bongos-worktrees";
/// The line in `info/exclude` that keeps [`WORKTREES_DIR`] out of `git status`.
pub const EXCLUDE_LINE: &str = "/.bongos-worktrees/";
/// The base of a new branch when the repository names no default branch.
const FALLBACK_BASE: &str = "main";

/// The id of the toast that reports a worktree made, apart from refusals.
pub struct StartedToast;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|w, _: &task::Start, window, cx| start(w, window, cx));
}

/// Whether a leaf of `kind` works in a worktree. A `document` or `other` leaf never does.
fn gets_worktree(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::PrShip | Kind::PrMerge | Kind::Spike | Kind::Babysit
    )
}

fn start(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Start which task?",
        "There is no open pr-ship, pr-merge, spike or babysit leaf to start",
        |task, tree| {
            is_open_leaf(task, tree) && task.file.as_ref().is_some_and(|f| gets_worktree(f.kind))
        },
        Box::new(|id, workspace, window, cx| start_task(&id, workspace, window, cx)),
    );
}

/// How the worktree's branch comes to be.
#[derive(Clone, Debug, PartialEq)]
enum Branch {
    /// A new branch cut from `base`, or from the repository's default branch when `None`.
    New { name: String, base: Option<String> },
    /// The head of PR `number` of the GitHub repository `github` (`owner/name`), fetched from
    /// the remote whose URL names that repository into the local branch `name`.
    PullRequest {
        name: String,
        github: String,
        number: u64,
    },
}

/// What `task: start` does for one task, decided from the tree and the settings alone.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    id: String,
    repository: Repository,
    /// The repository's primary checkout, with `~` expanded.
    repository_path: PathBuf,
    worktree: PathBuf,
    branch: Branch,
}

impl Request {
    /// The name of the new branch, when the request cuts one.
    #[cfg(test)]
    pub(crate) fn new_branch_name(&self) -> Option<&str> {
        match &self.branch {
            Branch::New { name, .. } => Some(name),
            Branch::PullRequest { .. } => None,
        }
    }
}

/// What the task should be started with, or the message of the toast that refuses or reports
/// instead. A babysit task gets its PRs to choose from.
pub(crate) enum Plan {
    Ready(Request),
    ChoosePullRequest { request: Request, urls: Vec<String> },
}

fn start_task(
    id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let repositories = WorkSettings::get_global(cx).repositories.clone();
    match plan(&tree, id, &repositories) {
        Err(message) => show_toast(workspace, message, cx),
        Ok(Plan::Ready(request)) => create(request, window, cx),
        Ok(Plan::ChoosePullRequest { request, urls }) => modals::pick(
            workspace,
            "Check out which PR?",
            urls.clone(),
            0,
            Box::new(move |choice, workspace, window, cx| {
                let Some(url) = urls.get(choice) else {
                    return;
                };
                let Some((github, number)) = pull_request(url) else {
                    show_toast(
                        workspace,
                        format!("{url} is not a GitHub pull request URL"),
                        cx,
                    );
                    return;
                };
                let request = Request {
                    branch: Branch::PullRequest {
                        name: format!("pr-{number}"),
                        github,
                        number,
                    },
                    ..request
                };
                create(request, window, cx);
            }),
            window,
            cx,
        ),
    }
}

/// Decides what `task: start` does for the task `id`, or why it does nothing.
pub(crate) fn plan(tree: &WorkTree, id: &str, repositories: &[Repository]) -> Result<Plan, String> {
    let place = Place::locate(tree, id)?;
    let slug = place.slug();
    let file = place
        .task
        .file
        .as_ref()
        .ok_or_else(|| format!("{slug} has no task.toml"))?;
    if derive::shape(place.task, tree) == Shape::Group {
        return Err(format!(
            "{slug} is a group; only a leaf task gets a worktree"
        ));
    }
    if !gets_worktree(file.kind) {
        return Err(format!(
            "{slug} is of kind {}; only pr-ship, pr-merge, spike and babysit tasks get a worktree",
            model::kind_name(file.kind)
        ));
    }
    if file.status != Status::Open {
        return Err(format!("{slug} is not open; reopen it first"));
    }
    if let Some(existing) = &file.worktree {
        return Err(if expand_home(existing).exists() {
            format!("{slug} already has its worktree at {existing}")
        } else {
            gone_worktree(slug, existing, file.branch.as_deref())
        });
    }
    if derive::state(place.task, tree) == State::Blocked {
        return Err(format!(
            "{slug} is Blocked; task: show blockers says what holds it back"
        ));
    }
    let repository = resolve_repository(tree, id, repositories).ok_or_else(|| {
        format!(
            "{slug} has no repository: name one on the task, a parent or the project, and list \
             it in work.repositories"
        )
    })?;
    let repository_path = expand_home(&repository.path);
    // A one-off's directory name carries its date; its slug does not.
    let slug = if place.project.is_none() && place.ancestors.is_empty() {
        oneoff_slug(slug)
    } else {
        slug
    };
    let held: Vec<PathBuf> = tree
        .tasks()
        .into_iter()
        .filter(|task| task.dir != place.task.dir)
        .filter_map(|task| task.file.as_ref()?.worktree.as_deref().map(expand_home))
        .collect();
    let (worktree, slug) = free_worktree_path(&repository_path, slug, &held);
    let request = |branch| Request {
        id: id.to_string(),
        repository: repository.clone(),
        repository_path: repository_path.clone(),
        worktree: worktree.clone(),
        branch,
    };
    if file.kind == Kind::Babysit {
        let urls = pull_request_urls(file);
        if urls.is_empty() {
            return Err(format!(
                "{} watches no PR yet; set its result to its PRs first",
                place.slug()
            ));
        }
        let placeholder = Branch::PullRequest {
            name: String::new(),
            github: String::new(),
            number: 0,
        };
        return Ok(Plan::ChoosePullRequest {
            request: request(placeholder),
            urls,
        });
    }
    let name = start_branch_name(&repository.branch_template, &slug, file, &place.ancestors);
    let base = match &file.stacked_on {
        Some(edge) => {
            let parent = place.sibling(&edge.task).and_then(parent_branch);
            Some(parent.ok_or_else(|| {
                format!(
                    "{} is stacked on {}, which has no branch yet; start {} first",
                    place.slug(),
                    edge.task,
                    edge.task
                )
            })?)
        }
        None => None,
    };
    Ok(Plan::Ready(request(Branch::New { name, base })))
}

/// What to do about a recorded worktree whose directory is gone: bring it back on its branch, or,
/// without a branch, forget it and start again.
fn gone_worktree(slug: &str, worktree: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!(
            "{slug} records the worktree {worktree}, which no longer exists; nothing was created. \
             To bring it back, run `git worktree prune` and then `git worktree add {worktree} \
             {branch}` in its repository"
        ),
        None => format!(
            "{slug} records the worktree {worktree}, which no longer exists; nothing was created. \
             Remove the `worktree` line from its task.toml and start it again"
        ),
    }
}

fn parent_branch(parent: &Task) -> Option<String> {
    parent.file.as_ref()?.branch.clone()
}

pub(crate) fn pull_request_urls(file: &TaskFile) -> Vec<String> {
    let Some(result) = &file.result else {
        return Vec::new();
    };
    match result.kind {
        ResultKind::Prs => result.urls.clone(),
        ResultKind::Pr => result.url.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The repository (`owner/name`) and number of a GitHub pull request URL such as
/// `https://github.com/o/r/pull/12/files`.
pub(crate) fn pull_request(url: &str) -> Option<(String, u64)> {
    let segments: Vec<&str> = url.split('/').collect();
    let pull = segments.iter().position(|segment| *segment == "pull")?;
    let number = segments.get(pull + 1)?.parse().ok()?;
    let owner = segments.get(pull.checked_sub(2)?)?;
    let name = segments.get(pull - 1)?;
    if owner.is_empty() || name.is_empty() || owner.contains(':') {
        return None;
    }
    Some((format!("{owner}/{name}"), number))
}

/// Whether the remote `url` (HTTPS, SSH or a path) names the GitHub repository `github`
/// (`owner/name`).
pub(crate) fn remote_names(url: &str, github: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url).to_lowercase();
    let github = github.to_lowercase();
    url.strip_suffix(&github)
        .is_some_and(|rest| rest.ends_with('/') || rest.ends_with(':'))
}

/// `path` with a leading `~` replaced by the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        util::paths::home_dir().clone()
    } else if let Some(rest) = path.strip_prefix("~/") {
        util::paths::home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// `path` with symbolic links resolved as far as it exists, so two spellings of one place
/// compare equal even before the last parts of it are created.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `<repository>/.bongos-worktrees/<slug>`, or with `-2`, `-3` and so on when another task's
/// worktree is there (slugs are unique only among siblings). Also returns the slug used.
pub(crate) fn free_worktree_path(
    repository: &Path,
    slug: &str,
    held: &[PathBuf],
) -> (PathBuf, String) {
    let held: Vec<PathBuf> = held.iter().map(|path| normalize(path)).collect();
    let dir = repository.join(WORKTREES_DIR);
    let mut number = 1u64;
    loop {
        let name = if number == 1 {
            slug.to_string()
        } else {
            format!("{slug}-{number}")
        };
        let path = dir.join(&name);
        if !held.contains(&normalize(&path)) {
            return (path, name);
        }
        number += 1;
    }
}

/// Whether `file` is a synced Linear link: an identifier and the `[linear_state]` record behind
/// it. A hand-set `linear` and a pending create (no identifier yet) are not.
fn is_synced_link(file: &TaskFile) -> bool {
    file.linear.is_some() && file.linear_state.is_some()
}

/// The branch a started leaf gets (D15). A leaf with a synced Linear link takes
/// [`linear_branch_segment`]. Any other leaf takes [`branch_name`] with its own `linear`, else the
/// nearest ancestor's hand-set one; an ancestor with a synced link is skipped, so a group's
/// identifier never appears in a leaf's branch.
fn start_branch_name(template: &str, slug: &str, file: &TaskFile, ancestors: &[&Task]) -> String {
    if is_synced_link(file)
        && let Some(identifier) = file.linear.as_deref()
    {
        return linear_branch_segment(identifier, template, slug);
    }
    let linear = file.linear.as_deref().or_else(|| {
        ancestors.iter().find_map(|task| {
            let file = task.file.as_ref()?;
            file.linear.as_deref().filter(|_| !is_synced_link(file))
        })
    });
    branch_name(template, slug, linear)
}

/// The branch from `template`: `{slug}` becomes `slug` and `{linear}` the Linear id. Without a
/// Linear id, the separators `{linear}` leaves dangling go too, and an empty name falls back to
/// the slug.
pub(crate) fn branch_name(template: &str, slug: &str, linear: Option<&str>) -> String {
    let expanded = template
        .replace("{slug}", slug)
        .replace("{linear}", linear.unwrap_or(""));
    if linear.is_some() || !template.contains("{linear}") {
        return expanded;
    }
    let segments: Vec<&str> = expanded
        .split('/')
        .map(|segment| segment.trim_matches(|c| c == '-' || c == '_' || c == '.'))
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        slug.to_string()
    } else {
        segments.join("/")
    }
}

/// The git dir every worktree of the repository at `repository` shares: `.git` when it is a
/// directory; for a linked worktree (a `.git` file naming its own git dir), the directory that
/// git dir's `commondir` file names.
pub(crate) fn common_git_dir(repository: &Path) -> Result<PathBuf> {
    let dot_git = repository.join(".git");
    let metadata = std::fs::metadata(&dot_git)
        .with_context(|| format!("{} is not a git repository", repository.display()))?;
    if metadata.is_dir() {
        return Ok(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("reading {}", dot_git.display()))?;
    let git_dir = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .with_context(|| format!("{} names no gitdir", dot_git.display()))?;
    let git_dir = repository.join(git_dir);
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(common) => Ok(git_dir.join(common.trim())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(git_dir),
        Err(error) => Err(error).with_context(|| format!("reading {}", git_dir.display())),
    }
}

/// Adds [`EXCLUDE_LINE`] to the common git dir's `info/exclude` unless it is there, creating the
/// file and `info/` when missing. `.gitignore` is never touched.
pub(crate) fn ensure_excluded(repository: &Path) -> Result<()> {
    let info = common_git_dir(repository)?.join("info");
    let exclude = info.join("exclude");
    let mut text = match std::fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", exclude.display())),
    };
    if text.lines().any(|line| line.trim() == EXCLUDE_LINE) {
        return Ok(());
    }
    std::fs::create_dir_all(&info).with_context(|| format!("creating {}", info.display()))?;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(EXCLUDE_LINE);
    text.push('\n');
    write_atomic(&exclude, text.as_bytes())
        .with_context(|| format!("writing {}", exclude.display()))
}

/// Makes (or finds, after a start that was not recorded) the worktree on the background
/// executor, records it on the task, then runs the repository's setup command in it.
fn create(request: Request, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(fs) = WorkStore::global(cx).map(|store| store.read(cx).fs()) else {
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let made = cx
            .background_spawn({
                let request = request.clone();
                async move { make_worktree(fs, &request).await }
            })
            .await;
        let started = match made {
            Ok(started) => started,
            Err(error) => return toast(&workspace, format!("{error:#}"), cx),
        };
        let id = request.id.clone();
        let write = workspace
            .update(cx, |workspace, cx| {
                queue(workspace, cx, {
                    let started = started.clone();
                    move |tree, ctx| model::record_worktree(tree, ctx, &id, started.clone())
                })
            })
            .ok()
            .flatten();
        let Some(write) = write else {
            return;
        };
        if let Err(error) = write.await {
            return toast(
                &workspace,
                format!(
                    "The worktree {} was created, but could not be recorded: {error}",
                    started.worktree
                ),
                cx,
            );
        }
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<StartedToast>(),
                        format!("Started {} in {}", started.branch, started.worktree),
                    ),
                    cx,
                )
            })
            .ok();
        if let Some(command) = request.repository.setup_command.clone() {
            run_setup(&workspace, command, &request.worktree, cx).await;
        }
    })
    .detach();
}

fn toast(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncWindowContext) {
    workspace
        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
        .ok();
}

/// Runs `command` in a terminal tab with `worktree` as its directory. A failure stays visible
/// in the terminal and gets a toast; the worktree stays either way.
async fn run_setup(
    workspace: &WeakEntity<Workspace>,
    command: String,
    worktree: &Path,
    cx: &mut AsyncWindowContext,
) {
    let label = format!("setup {}", worktree.display());
    let spawn = ::task::SpawnInTerminal {
        id: ::task::TaskId(label.clone()),
        full_label: label.clone(),
        label,
        command: Some(command.clone()),
        command_label: command.clone(),
        cwd: Some(worktree.to_path_buf()),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        show_summary: true,
        show_command: true,
        ..Default::default()
    };
    let Ok(exit) = workspace.update_in(cx, |workspace, window, cx| {
        workspace.spawn_in_terminal(spawn, window, cx)
    }) else {
        return;
    };
    let failure = match exit.await {
        None => Some("no terminal is available".to_string()),
        Some(Err(error)) => Some(format!("{error:#}")),
        Some(Ok(status)) if !status.success() => Some(status.to_string()),
        Some(Ok(_)) => None,
    };
    if let Some(failure) = failure {
        toast(
            workspace,
            format!(
                "The setup command `{command}` failed in {} ({failure}); the worktree stays",
                worktree.display()
            ),
            cx,
        );
    }
}

/// Creates the worktree `request` asks for, or finds it when an earlier start made it but did
/// not record it. Refuses a path or branch something else holds.
async fn make_worktree(fs: Arc<dyn Fs>, request: &Request) -> Result<StartedWorktree> {
    let repository_path = &request.repository_path;
    let git_binary = which::which("git").ok();
    let repository = fs
        .open_repo(&repository_path.join(".git"), git_binary.as_deref())
        .with_context(|| format!("{} is not a git repository", repository_path.display()))?;
    let branch = match &request.branch {
        Branch::New { name, .. } | Branch::PullRequest { name, .. } => name.clone(),
    };
    let branch_ref = format!("refs/heads/{branch}");
    let worktrees = repository.worktrees().await?;
    let on_branch =
        |worktree: &&Worktree| worktree.ref_name.as_deref() == Some(branch_ref.as_str());
    let worktree_path = request.worktree.to_string_lossy().into_owned();

    if std::fs::symlink_metadata(&request.worktree).is_ok() {
        let target = normalize(&request.worktree);
        let ours = worktrees
            .iter()
            .filter(on_branch)
            .any(|worktree| normalize(&worktree.path) == target);
        if !ours {
            bail!(
                "{worktree_path} already exists and is not a worktree of {} on {branch}; \
                 nothing was created",
                request.repository.name
            );
        }
        // The base moved on since the branch was cut, perhaps; where they meet is what it was
        // cut from.
        let (base, base_sha) = match &request.branch {
            Branch::New { base, .. } => {
                let base = resolve_base(repository.as_ref(), base.clone()).await;
                let base_sha = match (&git_binary, resolve_sha(repository.as_ref(), &base).await) {
                    (Some(git_binary), Some(tip)) => run_git(
                        git_binary,
                        repository_path,
                        &["merge-base", &tip, &branch_ref],
                    )
                    .await
                    .log_err(),
                    _ => None,
                };
                (Some(base), base_sha)
            }
            Branch::PullRequest { .. } => (None, None),
        };
        return Ok(StartedWorktree {
            branch,
            base,
            base_sha,
            worktree: worktree_path,
        });
    }
    let exists = repository
        .revparse_batch(vec![branch_ref.clone()])
        .await?
        .into_iter()
        .next()
        .flatten()
        .is_some();
    if exists {
        let place = worktrees
            .iter()
            .find(on_branch)
            .map(|worktree| format!(", checked out in {}", worktree.path.display()))
            .unwrap_or_default();
        if !matches!(request.branch, Branch::PullRequest { .. }) || !place.is_empty() {
            bail!("The branch {branch} already exists{place}; nothing was created");
        }
    }

    match &request.branch {
        Branch::New { base, .. } => {
            let base = resolve_base(repository.as_ref(), base.clone()).await;
            let base_sha = resolve_sha(repository.as_ref(), &base)
                .await
                .ok_or_else(|| {
                    anyhow!(
                        "{} has no branch {base} to start from",
                        request.repository.name
                    )
                })?;
            ensure_excluded(repository_path)?;
            repository
                .create_worktree(
                    CreateWorktreeTarget::NewBranch {
                        branch_name: branch.clone(),
                        base_sha: Some(base_sha.clone()),
                    },
                    request.worktree.clone(),
                )
                .await?;
            Ok(StartedWorktree {
                branch,
                base: Some(base),
                base_sha: Some(base_sha),
                worktree: worktree_path,
            })
        }
        Branch::PullRequest { github, number, .. } => {
            let git_binary =
                git_binary.context("git is not on PATH, so the PR cannot be fetched")?;
            let mut remotes: Vec<(String, String)> =
                repository.remote_urls().await.into_iter().collect();
            remotes.sort();
            let remote = remotes
                .into_iter()
                .find(|(_, url)| remote_names(url, github))
                .map(|(name, _)| name)
                .with_context(|| {
                    format!(
                        "No remote of {} points at {github}, so PR {number} cannot be fetched; \
                         nothing was created",
                        request.repository.name
                    )
                })?;
            let refspec = format!("pull/{number}/head:refs/heads/{branch}");
            // Not forced, so a local branch with commits the PR lacks is refused, not overwritten.
            run_git(&git_binary, repository_path, &["fetch", &remote, &refspec])
                .await
                .with_context(|| format!("Fetching PR {number} from {remote}"))?;
            ensure_excluded(repository_path)?;
            repository
                .create_worktree(
                    CreateWorktreeTarget::ExistingBranch {
                        branch_name: branch.clone(),
                    },
                    request.worktree.clone(),
                )
                .await?;
            Ok(StartedWorktree {
                branch,
                base: None,
                base_sha: None,
                worktree: worktree_path,
            })
        }
    }
}

/// `base`, else the repository's default branch, else `main`.
async fn resolve_base(repository: &dyn GitRepository, base: Option<String>) -> String {
    if let Some(base) = base {
        return base;
    }
    default_or_fallback(repository.default_branch(false).await)
}

/// The default branch git found, else `main`.
pub(crate) fn default_or_fallback(found: Result<Option<SharedString>>) -> String {
    match found {
        Ok(Some(branch)) => branch.to_string(),
        Ok(None) => FALLBACK_BASE.to_string(),
        Err(error) => {
            log::warn!("finding the default branch: {error:#}");
            FALLBACK_BASE.to_string()
        }
    }
}

/// The commit of the local branch `branch`, else of `origin/<branch>`.
pub(crate) async fn resolve_sha(repository: &dyn GitRepository, branch: &str) -> Option<String> {
    let revisions = vec![
        format!("refs/heads/{branch}"),
        format!("refs/remotes/origin/{branch}"),
    ];
    match repository.revparse_batch(revisions).await {
        Ok(shas) => shas.into_iter().flatten().next(),
        Err(error) => {
            log::warn!("resolving {branch}: {error:#}");
            None
        }
    }
}

/// Runs git with `args` in `directory` and returns its trimmed output, or its error output as
/// the error.
async fn run_git(git_binary: &Path, directory: &Path, args: &[&str]) -> Result<String> {
    let output = util::command::new_command(git_binary)
        .current_dir(directory)
        .args(args.iter().map(OsString::from))
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
