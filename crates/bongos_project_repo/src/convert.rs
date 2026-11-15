//! `project: convert`: turns a project of the old plain-file layout into a project repository.
//!
//! The pure plan ([`bongos_project_repo_model::convert`]) says what to make. Here the old
//! folder is checked again on disk ([`preview`] shows the result), then the whole new project
//! folder is built under `<projects dir>/.converting-<id>/`: the definitions with one commit,
//! the copied files, and the state store filled without any state apply hook. Only then is the
//! folder renamed into place and, last, the location record written with `converted_from`
//! naming the old folder. Any failure removes the temporary folder and everything else the
//! conversion made. The old folder is only ever read.
//!
//! A conversion holds an in-process claim on the old folder, so a second conversion of the
//! same project, or a start of work in it, is refused while it runs; and a lock beside its
//! temporary folder, so [`clean_stale_conversions`] never removes the folder of a conversion
//! that is still running in any process.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context as _, anyhow, bail};
use bongos_project_repo_model::convert::{
    CONVERSION_COMMIT_MESSAGE, CONVERTING_PREFIX, ConversionPlan, ConversionSummary, Rejected,
    Target, name_problem, plan, plan_oneoffs, relative_path,
};
use bongos_project_repo_model::{DefinitionTree, layout};
use bongos_work_model::layout as old_layout;
use bongos_work_model::store::{Activity, Project, WorkTree};
use bongos_work_model::{HistoryEntry, Timestamp, parse_history, serialize_project};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::create::{CREATION_COMMIT_KEY, LayoutTemplates, MAIN_BRANCH};
use crate::git::{Git, Identity};
use crate::locations::{Location, RepositoryLocations};
use crate::state::{ImportReason, StateStore};
use crate::state_hooks::StateApplyHooks;

/// The suffix of the lock file beside a conversion's temporary folder.
const LOCK_SUFFIX: &str = ".lock";

/// One old project to convert: a project folder, or the one-offs.
#[derive(Clone, Debug)]
pub enum OldSource<'a> {
    Project(&'a Project),
    /// The activities under `dir` (`oneoffs/`), into the project titled `title`; the groups
    /// the conversion makes are created `now`.
    Oneoffs {
        dir: PathBuf,
        activities: &'a [Activity],
        title: String,
        now: Timestamp,
    },
}

impl OldSource<'_> {
    /// The old folder: `projects/<slug>` or `oneoffs`.
    pub fn old_dir(&self) -> PathBuf {
        match self {
            OldSource::Project(project) => project.task.dir.clone(),
            OldSource::Oneoffs { dir, .. } => dir.clone(),
        }
    }

    fn plan(&self, new_id: &mut impl FnMut() -> String) -> ConversionPlan {
        match self {
            OldSource::Project(project) => plan(project, new_id),
            OldSource::Oneoffs {
                dir,
                activities,
                title,
                now,
            } => plan_oneoffs(dir, activities, title, *now, new_id),
        }
    }
}

/// Every old project of `tree` that no location record names as converted: each project, then
/// the one-offs (when there are any) into the project titled `oneoffs_title`.
pub fn unconverted_sources<'a>(
    tree: &'a WorkTree,
    locations: &RepositoryLocations,
    oneoffs_title: &str,
    now: Timestamp,
) -> Vec<OldSource<'a>> {
    let mut sources: Vec<OldSource<'a>> = tree
        .projects
        .iter()
        .filter(|project| locations.converted_from(&project.task.dir).is_none())
        .map(OldSource::Project)
        .collect();
    let dir = old_layout::oneoffs_dir(&tree.root);
    if !tree.activities.is_empty() && locations.converted_from(&dir).is_none() {
        sources.push(OldSource::Oneoffs {
            dir,
            activities: &tree.activities,
            title: oneoffs_title.to_string(),
            now,
        });
    }
    sources
}

/// Where and how conversions make their projects.
#[derive(Clone, Copy)]
pub struct ConvertOptions<'a> {
    /// Where project folders go.
    pub projects_dir: &'a Path,
    /// Every part must resolve inside the project folder: the folder is built whole and renamed
    /// into place.
    pub layout: &'a LayoutTemplates,
    /// The state apply hooks of the app. The new store's writer is opened with them, and the
    /// conversion runs none of them.
    pub hooks: &'a StateApplyHooks,
}

/// What converting one old project would do, shown before asking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionPreview {
    pub summary: ConversionSummary,
    /// The new project folder; `None` when the conversion is refused.
    pub destination: Option<PathBuf>,
}

impl ConversionPreview {
    /// Whether converting would be refused; `summary.blockers` says why.
    pub fn is_blocked(&self) -> bool {
        !self.summary.blockers.is_empty()
    }
}

/// A project converted.
#[derive(Clone, Debug)]
pub struct ConvertedProject {
    pub location: Location,
    pub summary: ConversionSummary,
}

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("{} is being converted or has work starting; try again when it ends", .0.display())]
    Busy(PathBuf),
    #[error("{} was already converted, into {}", .old.display(), .folder.display())]
    AlreadyConverted { old: PathBuf, folder: PathBuf },
    #[error("the project cannot be converted as it is: {}", .0.join("; "))]
    Blocked(Vec<String>),
    #[error("a project with the id {id} is already on this machine, at {}", .folder.display())]
    IdTaken { id: String, folder: PathBuf },
    #[error("{} already exists; the one-offs are not merged into an existing project", .0.display())]
    DestinationExists(PathBuf),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// A phase of a conversion, at which a test makes it fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Build,
    Commit,
    Import,
    Rename,
    LocationRecord,
    /// After the location record is written, as when only syncing its folder fails.
    AfterLocationRecord,
}

/// The test seams of a conversion.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Seams {
    pub fail_at: Option<Phase>,
}

impl Seams {
    fn check(&self, phase: Phase) -> anyhow::Result<()> {
        if self.fail_at == Some(phase) {
            bail!("failing at {phase:?} as the test asked");
        }
        Ok(())
    }
}

// Claims on old folders --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Converting,
    Started(usize),
}

static CLAIMS: LazyLock<Mutex<BTreeMap<PathBuf, Claim>>> = LazyLock::new(Default::default);

fn claim_key(old_dir: &Path) -> PathBuf {
    std::fs::canonicalize(old_dir).unwrap_or_else(|_| old_dir.to_path_buf())
}

/// A claim on an old folder, released when dropped: a conversion's, or one start of work.
#[derive(Debug)]
pub struct OldProjectClaim {
    key: PathBuf,
}

impl Drop for OldProjectClaim {
    fn drop(&mut self) {
        let mut claims = CLAIMS.lock();
        match claims.get(&self.key).copied() {
            Some(Claim::Started(count)) if count > 1 => {
                claims.insert(self.key.clone(), Claim::Started(count - 1));
            }
            _ => {
                claims.remove(&self.key);
            }
        }
    }
}

/// Claims the old folder `old_dir` (`projects/<slug>`, or `oneoffs` for a one-off) for a start
/// of work. Refused while it is being converted; any number of starts may hold it at once.
pub fn claim_old_project_start(old_dir: &Path) -> Result<OldProjectClaim, ConvertError> {
    let key = claim_key(old_dir);
    let mut claims = CLAIMS.lock();
    let next = match claims.get(&key) {
        Some(Claim::Converting) => return Err(ConvertError::Busy(old_dir.to_path_buf())),
        Some(Claim::Started(count)) => Claim::Started(count + 1),
        None => Claim::Started(1),
    };
    claims.insert(key.clone(), next);
    Ok(OldProjectClaim { key })
}

/// Whether the old folder `old_dir` is being converted now, in this process.
pub fn is_converting(old_dir: &Path) -> bool {
    CLAIMS.lock().get(&claim_key(old_dir)) == Some(&Claim::Converting)
}

/// Claims `old_dir` for a conversion; refused while another conversion or a start holds it.
pub(crate) fn claim_conversion(old_dir: &Path) -> Result<OldProjectClaim, ConvertError> {
    let key = claim_key(old_dir);
    let mut claims = CLAIMS.lock();
    if claims.contains_key(&key) {
        return Err(ConvertError::Busy(old_dir.to_path_buf()));
    }
    claims.insert(key.clone(), Claim::Converting);
    Ok(OldProjectClaim { key })
}

// Which old projects are converted ---------------------------------------------------------

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || claim_key(a) == claim_key(b)
}

/// Whether an old-format project still needs converting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OldFormatState {
    /// No location record names it: shown as "old format, needs conversion", writes refused.
    NeedsConversion,
    /// Converted into the project at `folder`: hidden from the Work view.
    Converted { folder: PathBuf },
}

impl RepositoryLocations {
    /// The location of the project converted from the old folder `old_dir`, if any.
    pub fn converted_from(&self, old_dir: &Path) -> Option<&Location> {
        self.locations().iter().find(|location| {
            location
                .converted_from
                .as_deref()
                .is_some_and(|from| same_path(from, old_dir))
        })
    }

    /// Whether the old folder `old_dir` still needs converting.
    pub fn old_format_state(&self, old_dir: &Path) -> OldFormatState {
        match self.converted_from(old_dir) {
            Some(location) => OldFormatState::Converted {
                folder: location.folder.clone(),
            },
            None => OldFormatState::NeedsConversion,
        }
    }
}

// Preview -----------------------------------------------------------------------------------

/// What converting `source` would do, checked on disk, without changing anything.
pub fn preview(
    source: &OldSource,
    options: ConvertOptions,
    locations: &RepositoryLocations,
) -> ConversionPreview {
    let mut plan = source.plan(&mut bongos_work_model::new_id);
    scan(&mut plan);
    let old_dir = source.old_dir();
    if let Some(location) = locations.converted_from(&old_dir) {
        plan.blockers.push(format!(
            "already converted, into {}",
            location.folder.display()
        ));
    }
    if is_converting(&old_dir) {
        plan.blockers
            .push("a conversion of this project is running".to_string());
    }
    if let Some(known) = locations.location(plan.project_id()) {
        plan.blockers.push(format!(
            "a project with the id {} is already on this machine, at {}",
            plan.project_id(),
            known.folder.display()
        ));
    }
    let destination = match destination(source, &plan, options.projects_dir, locations) {
        Ok(destination) => Some(destination),
        Err(error) => {
            plan.blockers.push(error.to_string());
            None
        }
    };
    let mut summary = plan.summary();
    if let Some(destination) = &destination {
        summary
            .creates
            .insert(0, format!("the project folder {}", destination.display()));
    }
    if summary.blockers.is_empty() {
        ConversionPreview {
            summary,
            destination,
        }
    } else {
        ConversionPreview {
            summary,
            destination: None,
        }
    }
}

/// The previews of every unconverted old project of `tree`.
pub fn preview_all(
    tree: &WorkTree,
    oneoffs_title: &str,
    now: Timestamp,
    options: ConvertOptions,
    locations: &RepositoryLocations,
) -> Vec<ConversionPreview> {
    unconverted_sources(tree, locations, oneoffs_title, now)
        .iter()
        .map(|source| preview(source, options, locations))
        .collect()
}

/// The new project folder: the old slug, or the first free `<slug>-<n>`. The one-offs go only
/// into a folder that does not exist.
fn destination(
    source: &OldSource,
    plan: &ConversionPlan,
    projects_dir: &Path,
    locations: &RepositoryLocations,
) -> Result<PathBuf, ConvertError> {
    let taken = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok()
            || locations
                .locations()
                .iter()
                .any(|location| location.folder == path)
    };
    let first = projects_dir.join(&plan.slug);
    if let OldSource::Oneoffs { .. } = source {
        return if taken(&first) {
            Err(ConvertError::DestinationExists(first))
        } else {
            Ok(first)
        };
    }
    for n in 1..=10_000u32 {
        let candidate = if n == 1 {
            first.clone()
        } else {
            projects_dir.join(format!("{}-{n}", plan.slug))
        };
        if !taken(&candidate) {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "no free folder name for {} in {}",
        plan.slug,
        projects_dir.display()
    )
    .into())
}

// The check on disk -------------------------------------------------------------------------

/// The entries of `history.old.toml`, by task id.
type Histories = BTreeMap<String, Vec<HistoryEntry>>;

enum Kind {
    Regular,
    Symlink,
    Missing,
    Other,
}

/// What `relative` (already checked by name) is under `dir`, never following a link: a link
/// anywhere on the way is a link.
fn kind_at(dir: &Path, relative: &str) -> Kind {
    let mut path = dir.to_path_buf();
    let parts: Vec<&str> = relative.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => return Kind::Missing,
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Kind::Symlink;
        }
        let last = index + 1 == parts.len();
        if last {
            return if kind.is_file() {
                Kind::Regular
            } else {
                Kind::Other
            };
        }
        if !kind.is_dir() {
            return Kind::Other;
        }
    }
    Kind::Other
}

/// Checks the old folder on disk against the plan: every planned file is a regular file reached
/// without a link, `history.old.toml` parses, and every other entry is named with why it is not
/// converted. Returns the old histories to fold.
fn scan(plan: &mut ConversionPlan) -> Histories {
    let mut histories = Histories::new();
    match std::fs::symlink_metadata(&plan.old_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            plan.blockers.push(format!(
                "{} is a symbolic link; convert the folder it points to",
                plan.old_dir.display()
            ));
            return histories;
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            plan.blockers
                .push(format!("{} is not a folder", plan.old_dir.display()));
            return histories;
        }
        Err(error) => {
            plan.blockers
                .push(format!("{}: {error}", plan.old_dir.display()));
            return histories;
        }
    }

    let root = plan.old_dir.clone();
    let mut rejected = Vec::new();
    let mut blockers = Vec::new();
    for task in &mut plan.tasks {
        if task.synthetic {
            scan_listing(
                &task.old_dir,
                &task.child_slugs,
                &mut rejected,
                &mut blockers,
            );
            continue;
        }
        let inside = task
            .old_dir
            .strip_prefix(&root)
            .ok()
            .filter(|rest| rest.as_os_str().is_empty() || relative_path(rest).is_ok());
        let Some(inside) = inside else {
            blockers.push(format!(
                "{} is not inside {}",
                task.old_dir.display(),
                root.display()
            ));
            continue;
        };
        if !inside.as_os_str().is_empty() {
            let relative = inside.to_string_lossy().replace('\\', "/");
            if !matches!(kind_at(&root, &relative), Kind::Other) {
                blockers.push(format!(
                    "{} is not a folder reached without a symbolic link",
                    task.old_dir.display()
                ));
                continue;
            }
        }

        let dir = task.old_dir.clone();
        task.copies.retain(|copy| match kind_at(&dir, &copy.from) {
            Kind::Regular => true,
            kind => {
                rejected.push(Rejected {
                    path: dir.join(&copy.from),
                    reason: kind_reason(&kind).to_string(),
                });
                false
            }
        });
        if task.fold_old_history {
            match kind_at(&dir, old_layout::HISTORY_OLD_FILE) {
                Kind::Regular => match read_history(&dir.join(old_layout::HISTORY_OLD_FILE)) {
                    Ok(entries) => {
                        histories.insert(task.id.clone(), entries);
                    }
                    Err(error) => blockers.push(format!("{error:#}")),
                },
                kind => {
                    task.fold_old_history = false;
                    task.consumed
                        .retain(|name| name != old_layout::HISTORY_OLD_FILE);
                    rejected.push(Rejected {
                        path: dir.join(old_layout::HISTORY_OLD_FILE),
                        reason: format!("{}; its history is not kept", kind_reason(&kind)),
                    });
                }
            }
        }

        let known: std::collections::BTreeSet<String> = task
            .copies
            .iter()
            .map(|copy| copy.from.clone())
            .chain(task.consumed.iter().cloned())
            .collect();
        scan_task_dir(&dir, "", &known, &mut rejected, &mut blockers);
        let children = dir.join(old_layout::TASKS_DIR);
        match std::fs::symlink_metadata(&children) {
            Ok(metadata) if metadata.is_dir() => {
                scan_listing(&children, &task.child_slugs, &mut rejected, &mut blockers);
            }
            Ok(_) => rejected.push(Rejected {
                path: children,
                reason: "not a folder of tasks; not copied".to_string(),
            }),
            Err(_) => {}
        }
    }
    plan.rejected.extend(rejected);
    plan.tidy_rejected();
    plan.blockers.extend(blockers);
    histories
}

fn kind_reason(kind: &Kind) -> &'static str {
    match kind {
        Kind::Regular => "a regular file",
        Kind::Symlink => "a symbolic link (or a path through one), never followed; not copied",
        Kind::Missing => "no longer there",
        Kind::Other => "not a regular file; not copied",
    }
}

fn read_history(path: &Path) -> anyhow::Result<Vec<HistoryEntry>> {
    let bytes = read_no_follow(path)?;
    let text =
        String::from_utf8(bytes).map_err(|_| anyhow!("{} is not UTF-8 text", path.display()))?;
    let file = parse_history(&text)
        .map_err(|error| anyhow!("{} does not parse: {error}", path.display()))?;
    Ok(file.history)
}

/// The entries of a folder holding only task folders (`tasks/`, an activity, `oneoffs/`): each
/// one that is not one of `slugs` is named.
fn scan_listing(
    dir: &Path,
    slugs: &[String],
    rejected: &mut Vec<Rejected>,
    blockers: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_str() == Some(old_layout::OP_FILE) {
            blockers.push(journal_blocker(&path));
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let is_child = name
            .to_str()
            .is_some_and(|name| slugs.iter().any(|slug| slug == name));
        if is_child && kind.is_dir() {
            continue;
        }
        let reason = if kind.is_symlink() {
            "a symbolic link, never followed; not copied"
        } else if name.to_str().is_none() {
            "the name is not UTF-8; not copied"
        } else {
            "not a task folder the old layout reads; not copied"
        };
        rejected.push(Rejected {
            path,
            reason: reason.to_string(),
        });
    }
}

fn journal_blocker(path: &Path) -> String {
    format!(
        "{}: an interrupted operation's journal; open the Work view so it finishes, then convert",
        path.display()
    )
}

/// Every entry under a task folder (`relative` inside it) that the plan does not copy or read.
fn scan_task_dir(
    dir: &Path,
    relative: &str,
    known: &std::collections::BTreeSet<String>,
    rejected: &mut Vec<Rejected>,
    blockers: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            rejected.push(Rejected {
                path,
                reason: "the name is not UTF-8; not copied".to_string(),
            });
            continue;
        };
        if relative.is_empty() && name == old_layout::TASKS_DIR {
            continue;
        }
        let entry_relative = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if name == old_layout::OP_FILE {
            blockers.push(journal_blocker(&path));
            continue;
        }
        if kind.is_symlink() {
            rejected.push(Rejected {
                path,
                reason: kind_reason(&Kind::Symlink).to_string(),
            });
            continue;
        }
        if let Some(problem) = name_problem(&name) {
            rejected.push(Rejected {
                path,
                reason: problem,
            });
            continue;
        }
        if old_layout::is_temp_file(&name) {
            rejected.push(Rejected {
                path,
                reason: "a temporary file of an interrupted write; not copied".to_string(),
            });
            continue;
        }
        if kind.is_dir() {
            scan_task_dir(&path, &entry_relative, known, rejected, blockers);
        } else if !kind.is_file() {
            rejected.push(Rejected {
                path,
                reason: kind_reason(&Kind::Other).to_string(),
            });
        } else if !known.contains(&entry_relative) {
            rejected.push(Rejected {
                path,
                reason: "appeared after the project was read; not copied".to_string(),
            });
        }
    }
}

// Reading and writing files ---------------------------------------------------------------

/// Opens `path` for reading without following a link at its last part.
fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn read_no_follow(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    open_no_follow(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(bytes)
}

/// `base` joined with `relative`, which must be a checked relative path.
fn checked_join(base: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    match relative_path(Path::new(relative)) {
        Ok(checked) if checked == relative => Ok(base.join(relative)),
        Ok(_) => bail!("refusing to write {relative:?}: not a plain relative path"),
        Err(reason) => bail!("refusing to write {relative:?}: {reason}"),
    }
}

/// Copies the old file `relative` of `dir` to `to`, which must not exist, keeping its
/// modification time. A link anywhere on the way fails the copy.
fn copy_old_file(dir: &Path, relative: &str, to: &Path) -> anyhow::Result<()> {
    let from = checked_join(dir, relative)?;
    if !matches!(kind_at(dir, relative), Kind::Regular) {
        bail!(
            "{} changed while converting: it is no longer a regular file reached without a link",
            from.display()
        );
    }
    let mut source =
        open_no_follow(&from).with_context(|| format!("reading {}", from.display()))?;
    let modified = source
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok();
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .with_context(|| format!("creating {}", to.display()))?;
    io::copy(&mut source, &mut target)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    if let Some(modified) = modified {
        target.set_modified(modified).ok();
    }
    Ok(())
}

fn write_new(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))
}

/// Renames `from` to `to`, failing when `to` exists (even as an empty folder).
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both strings are valid and NUL-terminated for the duration of the call.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOSYS)
        ) {
            return Err(error);
        }
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    std::fs::rename(from, to)
}

// Temporary folders -------------------------------------------------------------------------

/// The lock beside a conversion's temporary folder, held while it runs; dropping it removes the
/// lock file.
pub(crate) struct TempLock {
    path: PathBuf,
    _file: File,
}

impl TempLock {
    /// Takes the lock of the temporary folder `temp`; `None` while someone holds it.
    pub(crate) fn take(temp: &Path) -> io::Result<Option<Self>> {
        let mut name = temp.as_os_str().to_owned();
        name.push(LOCK_SUFFIX);
        let path = PathBuf::from(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: the descriptor is open for the duration of the call.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    return Ok(None);
                }
                return Err(error);
            }
        }
        Ok(Some(Self { path, _file: file }))
    }
}

impl Drop for TempLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Removes, from `projects_dir`, every `.converting-*` folder a conversion left behind when it
/// was killed (one whose lock nobody holds), and lock files with no folder. A conversion still
/// running, in any process, is left alone. Returns what was removed. Never follows a link: a
/// link with such a name is removed itself.
pub fn clean_stale_conversions(projects_dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(rest) = name.strip_prefix(CONVERTING_PREFIX) else {
            continue;
        };
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if let Some(id) = rest.strip_suffix(LOCK_SUFFIX) {
            // A lock file whose folder is gone, left by a conversion killed after removing it.
            let folder = projects_dir.join(format!("{CONVERTING_PREFIX}{id}"));
            if kind.is_file() && std::fs::symlink_metadata(&folder).is_err() {
                if let Ok(Some(lock)) = TempLock::take(&folder) {
                    drop(lock);
                    removed.push(path);
                }
            }
            continue;
        }
        if rest.is_empty() || name_problem(&name).is_some() {
            continue;
        }
        if kind.is_symlink() || kind.is_file() {
            if std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
            continue;
        }
        match TempLock::take(&path) {
            Ok(Some(lock)) => {
                if std::fs::remove_dir_all(&path).is_ok() {
                    log::info!("removed the unfinished conversion {}", path.display());
                    removed.push(path);
                }
                drop(lock);
            }
            Ok(None) => {}
            Err(error) => log::warn!("checking {}: {error}", path.display()),
        }
    }
    removed
}

// Converting ------------------------------------------------------------------------------

/// Converts `source` into a new project folder in `options.projects_dir` and records its
/// location with `converted_from`. A failure leaves nothing behind.
pub async fn convert(
    source: &OldSource<'_>,
    options: ConvertOptions<'_>,
    locations: &mut RepositoryLocations,
    git: &Git,
    new_id: impl FnMut() -> String,
) -> Result<ConvertedProject, ConvertError> {
    convert_with(source, options, locations, git, new_id, Seams::default()).await
}

/// Converts every unconverted old project of `tree`, one after another; each result is named
/// by its old folder. One failing does not stop the others.
pub async fn convert_all(
    tree: &WorkTree,
    oneoffs_title: &str,
    now: Timestamp,
    options: ConvertOptions<'_>,
    locations: &mut RepositoryLocations,
    git: &Git,
    mut new_id: impl FnMut() -> String,
) -> Vec<(PathBuf, Result<ConvertedProject, ConvertError>)> {
    let sources = unconverted_sources(tree, locations, oneoffs_title, now);
    let mut results = Vec::new();
    for source in &sources {
        let result = convert(source, options, locations, git, &mut new_id).await;
        results.push((source.old_dir(), result));
    }
    results
}

pub(crate) async fn convert_with(
    source: &OldSource<'_>,
    options: ConvertOptions<'_>,
    locations: &mut RepositoryLocations,
    git: &Git,
    mut new_id: impl FnMut() -> String,
    seams: Seams,
) -> Result<ConvertedProject, ConvertError> {
    let old_dir = source.old_dir();
    let _claim = claim_conversion(&old_dir)?;
    // What another command recorded since the caller loaded the record counts.
    locations
        .reload()
        .map_err(|error| ConvertError::Failed(error.into()))?;
    if let Some(location) = locations.converted_from(&old_dir) {
        return Err(ConvertError::AlreadyConverted {
            old: old_dir,
            folder: location.folder.clone(),
        });
    }
    let mut plan = source.plan(&mut new_id);
    let histories = scan(&mut plan);
    if plan.is_blocked() {
        return Err(ConvertError::Blocked(plan.blockers));
    }
    if let Some(known) = locations.location(plan.project_id()) {
        return Err(ConvertError::IdTaken {
            id: plan.project_id().to_string(),
            folder: known.folder.clone(),
        });
    }
    let projects_dir = options.projects_dir;
    let destination = destination(source, &plan, projects_dir, locations)?;
    if let Some(orphan) = unrecorded_folder(projects_dir, &plan.slug, locations) {
        log::warn!(
            "{} is in the projects folder but no location record names it; it may be left by a \
             conversion stopped before it recorded the project. Add it with project: add \
             existing, or remove it",
            orphan.display()
        );
    }
    let summary = plan.summary();

    let made_projects_dir = std::fs::symlink_metadata(projects_dir).is_err();
    std::fs::create_dir_all(projects_dir)
        .with_context(|| format!("creating {}", projects_dir.display()))?;
    let temp_name = format!("{CONVERTING_PREFIX}{}", new_id());
    if let Some(problem) = name_problem(&temp_name) {
        return Err(anyhow!("the temporary folder's name cannot be used: {problem}").into());
    }
    let temp = projects_dir.join(&temp_name);
    let lock = match TempLock::take(&temp) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Err(anyhow!("{} is in use", temp.display()).into()),
        Err(error) => {
            return Err(anyhow!(error)
                .context(format!("locking {}", temp.display()))
                .into());
        }
    };
    let mut renamed = false;
    let result = match std::fs::create_dir(&temp) {
        Ok(()) => {
            execute(
                &plan,
                histories,
                &temp,
                &destination,
                options,
                locations,
                git,
                &mut renamed,
                seams,
            )
            .await
        }
        Err(error) => Err(anyhow!(error).context(format!("creating {}", temp.display()))),
    };
    if result.is_err() {
        if renamed {
            std::fs::remove_dir_all(&destination).ok();
        }
        std::fs::remove_dir_all(&temp).ok();
    }
    drop(lock);
    if result.is_err() && made_projects_dir {
        std::fs::remove_dir(projects_dir).ok();
    }
    let location = result?;
    log::info!(
        "converted {} into {}",
        plan.old_dir.display(),
        location.folder.display()
    );
    Ok(ConvertedProject { location, summary })
}

/// The folder `<projects_dir>/<slug>` when it exists and no location record names it, as a
/// conversion stopped between moving its folder into place and recording it leaves.
pub(crate) fn unrecorded_folder(
    projects_dir: &Path,
    slug: &str,
    locations: &RepositoryLocations,
) -> Option<PathBuf> {
    let folder = projects_dir.join(slug);
    let is_dir = std::fs::symlink_metadata(&folder).is_ok_and(|metadata| metadata.is_dir());
    let recorded = locations
        .locations()
        .iter()
        .any(|location| same_path(&location.folder, &folder));
    (is_dir && !recorded).then_some(folder)
}

fn resolve(template: &str, folder: &Path) -> PathBuf {
    PathBuf::from(layout::resolve_template(
        template,
        &folder.to_string_lossy(),
    ))
}

/// The part `template` of the folder `temp` and of the folder `destination`, which must be the
/// same path inside each.
fn part(template: &str, temp: &Path, destination: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let in_temp = resolve(template, temp);
    let in_destination = resolve(template, destination);
    let inside = in_temp
        .strip_prefix(temp)
        .ok()
        .filter(|rest| !rest.as_os_str().is_empty() && relative_path(rest).is_ok());
    match inside {
        Some(rest) if in_destination == destination.join(rest) => Ok((in_temp, in_destination)),
        _ => bail!(
            "the layout puts {template} outside the project folder; a conversion builds the \
             whole folder first, so every part must be inside it"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    plan: &ConversionPlan,
    mut histories: Histories,
    temp: &Path,
    destination: &Path,
    options: ConvertOptions<'_>,
    locations: &mut RepositoryLocations,
    git: &Git,
    renamed: &mut bool,
    seams: Seams,
) -> anyhow::Result<Location> {
    let templates = options.layout;
    let (repository, final_repository) = part(&templates.repository, temp, destination)?;
    let (working_copies, final_working_copies) =
        part(&templates.working_copies, temp, destination)?;
    let (state, final_state) = part(&templates.state, temp, destination)?;
    let (files, final_files) = part(&templates.files, temp, destination)?;
    let temp_location = Location {
        id: plan.project_id().to_string(),
        folder: temp.to_path_buf(),
        repository: repository.clone(),
        working_copies: working_copies.clone(),
        state: state.clone(),
        files: files.clone(),
        remote: None,
        converted_from: None,
    };

    // The folder: definitions, copied files, nothing committed yet.
    for path in [&repository, &working_copies, &state, &files] {
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    }
    let definitions = repository.join(layout::DEFINITIONS_DIR);
    write_new(
        &repository.join(layout::GITATTRIBUTES_FILE),
        layout::GITATTRIBUTES_CONTENTS,
    )?;
    let project_text = serialize_project(&plan.project)
        .map_err(|error| anyhow!("writing {}: {error}", layout::PROJECT_FILE))?;
    write_new(&definitions.join(layout::PROJECT_FILE), &project_text)?;
    let mut has_plan = false;
    for task in &plan.tasks {
        let folder = if task.folder.is_empty() {
            definitions.clone()
        } else {
            checked_join(&definitions, &task.folder)?
        };
        let text = toml::to_string(&task.definition)
            .with_context(|| format!("writing the definition of task {}", task.id))?;
        write_new(&folder.join(layout::TASK_FILE), &text)?;
        if task.copies.is_empty() {
            continue;
        }
        let task_files = checked_join(&files, &task.id)?;
        for copy in &task.copies {
            let to = match &copy.to {
                Target::Definitions(path) => {
                    has_plan |= task.folder.is_empty() && path == layout::PLAN_FILE;
                    checked_join(&definitions, path)?
                }
                Target::Files(path) => checked_join(&task_files, path)?,
            };
            copy_old_file(&task.old_dir, &copy.from, &to)?;
        }
    }
    if !has_plan {
        write_new(&definitions.join(layout::PLAN_FILE), "")?;
    }
    let parsed = DefinitionTree::parse_dir(&definitions).map_err(|problems| {
        let list: Vec<String> = problems.iter().map(ToString::to_string).collect();
        anyhow!(
            "the converted definitions do not read back: {}",
            list.join("; ")
        )
    })?;
    let mut parsed_ids: Vec<&str> = parsed
        .nodes()
        .iter()
        .map(|node| node.definition.id.as_str())
        .collect();
    let mut planned_ids: Vec<&str> = plan.tasks.iter().map(|task| task.id.as_str()).collect();
    parsed_ids.sort_unstable();
    planned_ids.sort_unstable();
    if parsed_ids != planned_ids {
        bail!("the converted definitions do not hold every task");
    }
    seams.check(Phase::Build)?;

    // One commit.
    git.init_repo(&repository, MAIN_BRANCH).await?;
    let commit = git
        .commit_all(&repository, CONVERSION_COMMIT_MESSAGE, &Identity::default())
        .await?;
    seams.check(Phase::Commit)?;
    crate::working_copies::protect_files(&repository).context("making master read-only")?;

    // The state, without any state apply hook.
    {
        let writer = StateStore::open_writer_with_hooks(&temp_location, options.hooks)
            .map_err(|error| anyhow!("opening the new state store: {error}"))?;
        writer.import_without_hooks(ImportReason::Conversion, None, |tx| {
            for task in &plan.tasks {
                let mut state = task.state.clone();
                if let Some(mut old) = histories.remove(&task.id) {
                    old.append(&mut state.history);
                    state.history = old;
                }
                tx.set_task(&task.id, state)?;
            }
            tx.set_meta(CREATION_COMMIT_KEY, &commit)?;
            seams.check(Phase::Import)
        })?;
    }

    // Into place, then the location record last.
    seams.check(Phase::Rename)?;
    rename_no_replace(temp, destination).with_context(|| {
        format!(
            "moving the converted project into {}",
            destination.display()
        )
    })?;
    *renamed = true;
    let location = Location {
        id: plan.project_id().to_string(),
        folder: destination.to_path_buf(),
        repository: final_repository,
        working_copies: final_working_copies,
        state: final_state,
        files: final_files,
        remote: None,
        converted_from: Some(plan.old_dir.clone()),
    };
    seams.check(Phase::LocationRecord)?;
    let recorded = locations
        .record(location.clone())
        .map_err(anyhow::Error::from)
        .and_then(|()| seams.check(Phase::AfterLocationRecord));
    if let Err(error) = recorded {
        // The write may have reached the disk though it failed (syncing the folder after the
        // rename): take the entry out again, since the caller removes the folder it names.
        if let Err(forget_error) = locations.forget(&location) {
            log::warn!(
                "removing the location record of {} after a failed conversion: {forget_error}",
                location.folder.display()
            );
        }
        return Err(error);
    }
    Ok(location)
}
