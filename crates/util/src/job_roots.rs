//! A registry of job roots: directories written by sandboxed jobs, whose git data the editor must
//! never open.
//!
//! The registry is a JSON file (`roots.json`) shared by every process of the app. Writers take an
//! exclusive lock on `roots.lock` beside it, re-read the file, change it, and replace it through a
//! synced temporary file and a rename. Readers keep an in-memory copy and re-read the file, under
//! a shared lock, only when its inode, size or modification time changed.
//!
//! An entry covers its root and everything below it, except the paths in its `excluded` list (a
//! main worktree's masked `.git`, for example). A job-lifetime entry belongs to a running job; once
//! its owner process is gone, the next writer retires it. Retiring an entry walks its root for
//! nested repositories and registers each one permanently, so a repository a job created stays
//! refused after the job ends.

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// How long a registered root stays registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootLifetime {
    /// A tree or scratch directory the app made for a job. Stays until it is retired explicitly
    /// or the directory is gone.
    Permanent,
    /// A user's main worktree while a job runs in it. Stays while the owner process (identified by
    /// its pid and start time, so a reused pid does not count) is alive.
    Job { pid: u32, start_time: u64 },
}

/// Decides whether the owner of a job-lifetime entry is still alive, given its pid and start
/// time.
pub type OwnerCheck = Arc<dyn Fn(u32, u64) -> bool + Send + Sync>;

/// Stops a retire walk after this many directory entries; past it the entry is kept permanently
/// instead, which refuses more than needed but never less.
const MAX_WALK_ENTRIES: usize = 200_000;
/// Stops a retire walk below this depth, with the same fallback.
const MAX_WALK_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    root: PathBuf,
    #[serde(default)]
    excluded: Vec<PathBuf>,
    lifetime: RootLifetime,
}

impl Entry {
    fn covers(&self, canonical_path: &Path) -> bool {
        canonical_path.starts_with(&self.root)
            && !self
                .excluded
                .iter()
                .any(|excluded| canonical_path.starts_with(excluded))
    }
}

#[derive(Default, Serialize, Deserialize)]
struct RootsFile {
    #[serde(default)]
    roots: Vec<Entry>,
}

/// What identifies one version of `roots.json`: every write replaces the file by a rename, so a
/// new version has a new inode. The cache keeps the cached version open, so its inode cannot be
/// freed and reused by a later version while it is cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    inode: u64,
    size: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(metadata);
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            inode,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

#[derive(Default)]
struct Cache {
    stamp: Option<Stamp>,
    /// The open version of the file `stamp` describes, held so its inode stays allocated.
    file: Option<File>,
    entries: Arc<Vec<Entry>>,
}

/// One version of the file as read: its entries, its stamp and the open handle (both `None` when
/// the file does not exist).
struct Version {
    entries: Vec<Entry>,
    stamp: Option<Stamp>,
    file: Option<File>,
}

/// One view of a registry file. Several instances may share a file, in one process or many.
pub struct JobRoots {
    path: PathBuf,
    lock_path: PathBuf,
    owner_alive: OwnerCheck,
    cache: RwLock<Cache>,
}

impl JobRoots {
    /// A registry stored at `path` (`roots.json`), locked through `roots.lock` beside it. Owner
    /// liveness is read from the operating system.
    pub fn new(path: &Path) -> Self {
        Self::with_owner_check(path, Arc::new(process_alive))
    }

    /// Like [`JobRoots::new`], with the owner liveness check replaced.
    pub fn with_owner_check(path: &Path, owner_alive: OwnerCheck) -> Self {
        Self {
            path: path.to_path_buf(),
            lock_path: path.with_file_name("roots.lock"),
            owner_alive,
            cache: RwLock::new(Cache::default()),
        }
    }

    /// Removes entries whose root no longer exists and retires entries whose owner is gone. Does
    /// nothing when the file does not exist.
    pub fn prune(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        self.modify(false, |entries| {
            // By design: an entry whose root is gone has nothing left to protect, so it is
            // dropped.
            entries.retain(|entry| entry.root.exists());
            Ok(())
        })
    }

    /// Registers `root`, except the paths in `excluded`, which must exist. A job-lifetime entry
    /// replaces an earlier job-lifetime entry for the same root; a permanent entry is never
    /// replaced, the new entry is added beside it.
    pub fn register(
        &self,
        root: &Path,
        excluded: &[PathBuf],
        lifetime: RootLifetime,
    ) -> Result<()> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("canonicalizing job root {}", root.display()))?;
        let excluded = excluded
            .iter()
            .map(|path| {
                fs::canonicalize(path)
                    .with_context(|| format!("canonicalizing excluded path {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let entry = Entry {
            root,
            excluded,
            lifetime,
        };
        self.modify(true, move |entries| {
            if matches!(entry.lifetime, RootLifetime::Job { .. }) {
                entries.retain(|existing| {
                    existing.root != entry.root
                        || !matches!(existing.lifetime, RootLifetime::Job { .. })
                });
            }
            if !entries.contains(&entry) {
                entries.push(entry);
            }
            Ok(())
        })
    }

    /// Retires every entry for `root`, registering each nested repository inside it
    /// permanently. Does nothing when `root` is not registered; fails when `root` cannot be
    /// canonicalized.
    pub fn retire(&self, root: &Path) -> Result<()> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("canonicalizing job root {}", root.display()))?;
        if !self.path.exists() {
            return Ok(());
        }
        self.modify(false, |entries| {
            let (retired, kept): (Vec<_>, Vec<_>) =
                entries.drain(..).partition(|entry| entry.root == root);
            *entries = kept;
            for entry in retired {
                retire_entry(entry, entries);
            }
            Ok(())
        })
    }

    /// Whether `path` is inside a registered root (and outside that root's excluded paths). A path
    /// that cannot be canonicalized counts as inside, unless the registry is empty.
    pub fn contains(&self, path: &Path) -> bool {
        let entries = self.current_entries();
        if entries.is_empty() {
            return false;
        }
        match fs::canonicalize(path) {
            Ok(path) => entries.iter().any(|entry| entry.covers(&path)),
            Err(error) => {
                log::warn!(
                    "job roots: treating {} as inside a job root, it cannot be canonicalized: {error}",
                    path.display()
                );
                true
            }
        }
    }

    fn current_entries(&self) -> Arc<Vec<Entry>> {
        let stamp = match fs::metadata(&self.path) {
            Ok(metadata) => Some(Stamp::of(&metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                log::error!(
                    "job roots: cannot stat {}, keeping the last copy: {error}",
                    self.path.display()
                );
                return self.read_cache().entries.clone();
            }
        };
        {
            let cache = self.read_cache();
            if cache.stamp == stamp {
                return cache.entries.clone();
            }
        }
        if stamp.is_none() {
            // The old handle is closed after the lock is released.
            let _old = std::mem::take(&mut *self.write_cache());
            return Arc::default();
        }
        match self.read_shared() {
            Ok(version) => {
                let entries = Arc::new(version.entries);
                let _old_file = {
                    let mut cache = self.write_cache();
                    cache.stamp = version.stamp;
                    cache.entries = entries.clone();
                    std::mem::replace(&mut cache.file, version.file)
                };
                entries
            }
            Err(ReadError::Corrupt(stamp, file, error)) => {
                log::error!(
                    "job roots: {} is corrupt, keeping the last copy: {error:#}",
                    self.path.display()
                );
                let (entries, _old_file) = {
                    let mut cache = self.write_cache();
                    cache.stamp = Some(stamp);
                    let old_file = std::mem::replace(&mut cache.file, Some(file));
                    (cache.entries.clone(), old_file)
                };
                entries
            }
            Err(ReadError::Io(error)) => {
                log::error!(
                    "job roots: cannot re-read {}, keeping the last copy: {error:#}",
                    self.path.display()
                );
                self.read_cache().entries.clone()
            }
        }
    }

    fn read_cache(&self) -> std::sync::RwLockReadGuard<'_, Cache> {
        self.cache.read().unwrap_or_else(|error| error.into_inner())
    }

    fn write_cache(&self) -> std::sync::RwLockWriteGuard<'_, Cache> {
        self.cache
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn open_lock_file(&self) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("opening {}", self.lock_path.display()))
    }

    /// Reads the file under a shared lock. A missing file is an empty list.
    fn read_shared(&self) -> std::result::Result<Version, ReadError> {
        let lock = self.open_lock_file().map_err(ReadError::Io)?;
        lock.lock_shared()
            .with_context(|| format!("locking {}", self.lock_path.display()))
            .map_err(ReadError::Io)?;
        let result = self.read_file();
        lock.unlock()
            .with_context(|| format!("unlocking {}", self.lock_path.display()))
            .map_err(ReadError::Io)?;
        result
    }

    fn read_file(&self) -> std::result::Result<Version, ReadError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Version {
                    entries: Vec::new(),
                    stamp: None,
                    file: None,
                });
            }
            Err(error) => {
                return Err(ReadError::Io(
                    anyhow!(error).context(format!("opening {}", self.path.display())),
                ));
            }
        };
        let stamp = Stamp::of(
            &file
                .metadata()
                .with_context(|| format!("reading metadata of {}", self.path.display()))
                .map_err(ReadError::Io)?,
        );
        let mut text = String::new();
        io::Read::read_to_string(&mut file, &mut text)
            .with_context(|| format!("reading {}", self.path.display()))
            .map_err(ReadError::Io)?;
        match serde_json::from_str::<RootsFile>(&text) {
            Ok(contents) => Ok(Version {
                entries: contents.roots,
                stamp: Some(stamp),
                file: Some(file),
            }),
            Err(error) => Err(ReadError::Corrupt(stamp, file, anyhow!(error))),
        }
    }

    /// Runs `change` on the entries under the exclusive lock, after retiring dead owners, and
    /// writes the result back when it differs.
    fn modify(
        &self,
        create_directory: bool,
        change: impl FnOnce(&mut Vec<Entry>) -> Result<()>,
    ) -> Result<()> {
        if create_directory && let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        let lock = self.open_lock_file()?;
        lock.lock()
            .with_context(|| format!("locking {}", self.lock_path.display()))?;
        let result = self.modify_locked(change);
        let unlocked = lock
            .unlock()
            .with_context(|| format!("unlocking {}", self.lock_path.display()));
        result?;
        unlocked
    }

    fn modify_locked(&self, change: impl FnOnce(&mut Vec<Entry>) -> Result<()>) -> Result<()> {
        let (mut entries, file_exists) = match self.read_file() {
            Ok(version) => (version.entries, version.stamp.is_some()),
            Err(ReadError::Io(error)) => return Err(error),
            // By design: the entries of a corrupt file cannot be trusted, so they are dropped and
            // the file is set aside for inspection.
            Err(ReadError::Corrupt(_, _, error)) => {
                self.set_corrupt_file_aside(&error)?;
                (Vec::new(), false)
            }
        };
        let before = entries.clone();

        let mut index = 0;
        while index < entries.len() {
            if let RootLifetime::Job { pid, start_time } = entries[index].lifetime
                && !(self.owner_alive)(pid, start_time)
            {
                let entry = entries.remove(index);
                log::info!(
                    "job roots: retiring {}, its owner {pid} is gone",
                    entry.root.display()
                );
                retire_entry(entry, &mut entries);
            } else {
                index += 1;
            }
        }

        change(&mut entries)?;
        if entries != before || (!file_exists && !entries.is_empty()) {
            self.write_file(&entries)?;
        }
        Ok(())
    }

    fn set_corrupt_file_aside(&self, error: &anyhow::Error) -> Result<()> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let mut aside = self.path.clone().into_os_string();
        aside.push(format!(".corrupt-{seconds}"));
        let aside = PathBuf::from(aside);
        log::error!(
            "job roots: {} is corrupt ({error:#}), moving it to {} and starting empty",
            self.path.display(),
            aside.display()
        );
        fs::rename(&self.path, &aside).with_context(|| {
            format!(
                "moving corrupt {} to {}",
                self.path.display(),
                aside.display()
            )
        })
    }

    fn write_file(&self, entries: &[Entry]) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("job roots file has no parent directory")?;
        let text = serde_json::to_string_pretty(&RootsFile {
            roots: entries.to_vec(),
        })?;
        let mut temporary = tempfile::Builder::new()
            .prefix("roots.json.")
            .suffix(".tmp")
            .tempfile_in(directory)
            .with_context(|| format!("creating a temporary file in {}", directory.display()))?;
        temporary
            .write_all(text.as_bytes())
            .context("writing the temporary job roots file")?;
        temporary
            .as_file()
            .sync_all()
            .context("syncing the temporary job roots file")?;
        temporary
            .persist(&self.path)
            .map_err(|error| anyhow!(error.error))
            .with_context(|| format!("renaming the temporary file over {}", self.path.display()))?;
        #[cfg(unix)]
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing {}", directory.display()))?;
        Ok(())
    }
}

enum ReadError {
    Io(anyhow::Error),
    Corrupt(Stamp, File, anyhow::Error),
}

/// Registers each nested repository inside `entry`'s root permanently (into `entries`). When the
/// walk cannot finish, keeps the whole entry permanently instead.
fn retire_entry(entry: Entry, entries: &mut Vec<Entry>) {
    if !entry.root.exists() {
        return;
    }
    match nested_repositories(&entry) {
        Ok(nested) => {
            for root in nested {
                let entry = Entry {
                    root,
                    excluded: Vec::new(),
                    lifetime: RootLifetime::Permanent,
                };
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
        }
        Err(error) => {
            log::warn!(
                "job roots: cannot walk {}, keeping it registered permanently: {error:#}",
                entry.root.display()
            );
            entries.push(Entry {
                lifetime: RootLifetime::Permanent,
                ..entry
            });
        }
    }
}

/// The repositories inside `entry`'s root, outside its excluded paths: each directory holding a
/// `.git` file or directory, and each directory shaped like a git directory under any name (a
/// bare repository, or the target of a `.git` file). Does not follow symbolic links.
fn nested_repositories(entry: &Entry) -> Result<Vec<PathBuf>> {
    let mut nested = Vec::new();
    let mut visited = 0;
    let mut walk = walkdir::WalkDir::new(&entry.root)
        .follow_links(false)
        .max_depth(MAX_WALK_DEPTH)
        .into_iter();
    while let Some(item) = walk.next() {
        let item = item?;
        visited += 1;
        if visited > MAX_WALK_ENTRIES {
            anyhow::bail!("more than {MAX_WALK_ENTRIES} entries");
        }
        if item.depth() == MAX_WALK_DEPTH && item.file_type().is_dir() {
            anyhow::bail!("deeper than {MAX_WALK_DEPTH} directories");
        }
        let is_dir = item.file_type().is_dir();
        if entry
            .excluded
            .iter()
            .any(|excluded| excluded == item.path())
        {
            if is_dir {
                walk.skip_current_dir();
            }
            continue;
        }
        if item.file_name() == ".git" {
            if is_dir {
                walk.skip_current_dir();
            }
            if let Some(parent) = item.path().parent() {
                nested.push(parent.to_path_buf());
            }
        } else if is_dir && looks_like_git_directory(item.path()) {
            walk.skip_current_dir();
            nested.push(item.path().to_path_buf());
        }
    }
    Ok(nested)
}

/// Whether `directory` holds `HEAD`, `objects` and `refs`, the entries git looks for in a git
/// directory. Any file type counts, so a symbolic link does too.
fn looks_like_git_directory(directory: &Path) -> bool {
    ["HEAD", "objects", "refs"]
        .iter()
        .all(|name| fs::symlink_metadata(directory.join(name)).is_ok())
}

/// The start time of process `pid` (field 22 of `/proc/<pid>/stat`, in clock ticks since boot),
/// or `None` when it is gone or unknown.
pub fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_start_time(&text)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Field 22 of `/proc/<pid>/stat`. The command name (field 2) is in parentheses and may hold
/// spaces and parentheses, so fields are counted after the last `)`.
#[cfg(target_os = "linux")]
fn parse_start_time(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether process `pid` is alive and started at `start_time`. Where that cannot be told, the
/// owner counts as alive, so its root stays refused.
pub fn process_alive(pid: u32, start_time: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => parse_start_time(&text).is_none_or(|started| started == start_time),
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, start_time);
        true
    }
}

static GLOBAL: RwLock<Option<Arc<JobRoots>>> = RwLock::new(None);

fn global() -> Option<Arc<JobRoots>> {
    GLOBAL
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Makes the file at `path` this process's registry, then prunes it (entries whose root is gone,
/// job-lifetime entries whose owner is gone). The registry is installed even when pruning fails.
pub fn init(path: &Path) -> Result<()> {
    let roots = Arc::new(JobRoots::new(path));
    *GLOBAL.write().unwrap_or_else(|error| error.into_inner()) = Some(roots.clone());
    roots.prune()
}

/// Registers `root` in this process's registry. See [`JobRoots::register`].
pub fn register(root: &Path, excluded: &[PathBuf], lifetime: RootLifetime) -> Result<()> {
    global()
        .context("the job roots registry is not initialized")?
        .register(root, excluded, lifetime)
}

/// Retires `root` from this process's registry. See [`JobRoots::retire`].
pub fn retire(root: &Path) -> Result<()> {
    global()
        .context("the job roots registry is not initialized")?
        .retire(root)
}

/// Whether `path` is inside a root of this process's registry; `false` before [`init`].
pub fn contains(path: &Path) -> bool {
    global().is_some_and(|roots| roots.contains(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_registered_by_second_registry_is_seen_by_first() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("registry").join("roots.json");
        let root = directory.path().join("tree");
        fs::create_dir_all(root.join("src")).unwrap();

        let first = JobRoots::new(&file);
        let second = JobRoots::new(&file);
        assert!(!first.contains(&root.join("src")));

        second
            .register(&root, &[], RootLifetime::Permanent)
            .unwrap();
        assert!(first.contains(&root.join("src")));
        assert!(!first.contains(directory.path()));

        second.retire(&root).unwrap();
        assert!(!first.contains(&root.join("src")));
    }

    #[test]
    fn test_register_never_replaces_a_permanent_entry() {
        let directory = tempfile::tempdir().unwrap();
        let roots = JobRoots::with_owner_check(
            &directory.path().join("registry").join("roots.json"),
            Arc::new(|_, _| true),
        );
        let root = directory.path().join("tree");
        fs::create_dir_all(root.join("masked")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        roots.register(&root, &[], RootLifetime::Permanent).unwrap();
        let job = RootLifetime::Job {
            pid: 1,
            start_time: 1,
        };
        roots.register(&root, &[root.join("masked")], job).unwrap();
        assert!(roots.contains(&root.join("masked")));

        let second_job = RootLifetime::Job {
            pid: 2,
            start_time: 2,
        };
        roots
            .register(&root, &[root.join("masked")], second_job)
            .unwrap();
        let entries = roots.current_entries();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.lifetime == RootLifetime::Permanent)
        );
        assert!(entries.iter().any(|entry| entry.lifetime == second_job));
        assert!(roots.contains(&root.join("masked")));
        assert!(roots.contains(&root.join("src")));
    }

    #[test]
    fn test_register_refuses_an_excluded_path_that_does_not_exist() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("registry").join("roots.json");
        let roots = JobRoots::new(&file);
        let root = directory.path().join("tree");
        fs::create_dir_all(&root).unwrap();

        assert!(
            roots
                .register(&root, &[root.join("later")], RootLifetime::Permanent)
                .is_err()
        );
        assert!(!file.exists());
        fs::create_dir_all(root.join("later")).unwrap();
        assert!(!roots.contains(&root.join("later")));
    }

    #[test]
    fn test_retire_of_a_missing_root_fails() {
        let directory = tempfile::tempdir().unwrap();
        let roots = JobRoots::new(&directory.path().join("registry").join("roots.json"));
        let root = directory.path().join("tree");
        fs::create_dir_all(&root).unwrap();
        roots.register(&root, &[], RootLifetime::Permanent).unwrap();

        assert!(roots.retire(&directory.path().join("missing")).is_err());
        assert!(roots.contains(&root));
    }

    #[test]
    fn test_same_size_rewrites_are_each_seen() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("registry").join("roots.json");
        let reader = JobRoots::new(&file);
        let writer = JobRoots::new(&file);
        let first = directory.path().join("tree-a");
        let second = directory.path().join("tree-b");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        for _ in 0..20 {
            writer
                .register(&first, &[], RootLifetime::Permanent)
                .unwrap();
            assert!(reader.contains(&first));
            assert!(!reader.contains(&second));
            writer.retire(&first).unwrap();
            writer
                .register(&second, &[], RootLifetime::Permanent)
                .unwrap();
            assert!(!reader.contains(&first));
            assert!(reader.contains(&second));
            writer.retire(&second).unwrap();
        }
    }
}
