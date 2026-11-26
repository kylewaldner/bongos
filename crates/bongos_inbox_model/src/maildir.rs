//! The inbox directory: a Maildir-style tree of item files.
//!
//! The root holds `tmp/`, `new/`, `cur/` and `archive/`. A delivered item is
//! `new/<unique>`; once its state is touched it is `<unique>:2,<flags>` in
//! `cur/` or `archive/`. An item is read when its name carries the `S` flag;
//! anything in `new/` is unread, and `archive/` holds dismissed items.
//!
//! Every state change gives the file a second name with a hard link and then
//! unlinks the first. A file is never written, overwritten or opened for
//! writing once delivered, and a hard link never replaces an existing name.
//! A crash between the link and the unlink leaves two names for one inode,
//! which [`scan`] lists once. [`recover`] unlinks the earlier name of a pair
//! in different directories; a pair in one directory (`cur/`) stays until the
//! next read or unread of that item resolves it.
//!
//! A move holds an advisory lock on the file from its link to its unlink, and
//! removing one name of a same-directory pair takes the same lock and checks
//! again that both names still point at the file, so two processes resolving
//! one pair in opposite directions never unlink both names.

use crate::format::{self, Item, Unreadable};
use crate::name::{self, Flags, ParsedName, SEEN};
use std::collections::{HashMap, HashSet};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

pub const TMP: &str = "tmp";
pub const NEW: &str = "new";
pub const CUR: &str = "cur";
pub const ARCHIVE: &str = "archive";

/// How many numbered `.<n>` names an archive collision tries before giving up.
const MAX_ARCHIVE_SUFFIX: u32 = 10_000;
/// How many fresh unique names delivery tries when one is taken.
const DELIVER_ATTEMPTS: u32 = 8;

/// The directory an item name lives in, ordered by how far along it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    New,
    Cur,
    Archive,
}

impl Dir {
    pub fn name(self) -> &'static str {
        match self {
            Dir::New => NEW,
            Dir::Cur => CUR,
            Dir::Archive => ARCHIVE,
        }
    }
}

/// Creates the root and its four subdirectories (mode 0700) when missing.
pub fn ensure_layout(root: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(root)?;
    for sub in [TMP, NEW, CUR, ARCHIVE] {
        builder.create(root.join(sub))?;
    }
    Ok(())
}

/// One item file found by a scan. Read state comes from the name alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub dir: Dir,
    pub unique: String,
    /// `None` for a name without a `:2,` suffix.
    pub flags: Option<Flags>,
    pub path: PathBuf,
    pub inode: u64,
    pub mtime: SystemTime,
    pub len: u64,
    dev: u64,
}

impl Entry {
    /// Builds an entry without touching the file system, for tests.
    #[cfg(test)]
    pub(crate) fn for_test(dir: Dir, file_name: &str, inode: u64, mtime: SystemTime) -> Entry {
        let parsed = name::parse_name(file_name).expect("valid test name");
        Entry {
            dir,
            unique: parsed.unique,
            flags: parsed.flags,
            path: PathBuf::from(dir.name()).join(file_name),
            inode,
            mtime,
            len: 0,
            dev: 0,
        }
    }

    pub fn parsed_name(&self) -> ParsedName {
        ParsedName {
            unique: self.unique.clone(),
            flags: self.flags.clone(),
        }
    }

    pub fn file_name(&self) -> String {
        self.parsed_name().render()
    }

    /// Anything in `new/` is unread; elsewhere the `S` flag decides.
    pub fn is_read(&self) -> bool {
        self.dir != Dir::New && self.parsed_name().is_read()
    }

    fn flags_string(&self) -> String {
        self.flags
            .as_ref()
            .map(Flags::as_string)
            .unwrap_or_default()
    }

    /// Among names sharing an inode the greatest key is listed: archive over
    /// cur over new. Two `cur/` names for one inode come from a read or unread
    /// that was interrupted (or is still running), and which one is the
    /// destination cannot be told from the names, so the scan deterministically
    /// lists the greatest flags string (the read name, when the two differ only
    /// by `S`). [`recover`] never unlinks either of them.
    fn keep_key(&self) -> (Dir, String, String) {
        (self.dir, self.flags_string(), self.file_name())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Also return the items in `archive/`.
    pub include_archive: bool,
}

/// Lists the items in `new/` and `cur/`; see [`scan_with`].
pub fn scan(root: &Path) -> io::Result<Vec<Entry>> {
    scan_with(root, ScanOptions::default())
}

/// Lists the items in `new/` and `cur/` (and `archive/` on request), sorted
/// by path. `tmp/` is never read. Names starting with `.`, names with a
/// malformed `:2,` suffix, directories and other non-regular files are
/// skipped. When a crash left two names for one inode only the one that is
/// furthest along is listed (see [`Entry::keep_key`]).
pub fn scan_with(root: &Path, options: ScanOptions) -> io::Result<Vec<Entry>> {
    let (keep, _) = resolve(collect(root, options.include_archive)?);
    let mut entries: Vec<Entry> = keep
        .into_iter()
        .filter(|entry| options.include_archive || entry.dir != Dir::Archive)
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Unlinks the extra names a crash between link and unlink left behind and
/// returns their paths. Only a name in an earlier directory than the kept one
/// (`new/` before `cur/` before `archive/`) is unlinked: every move goes
/// forward or stays in its directory and links before it unlinks, so the
/// later name always still exists or has a successor, even while another
/// process is moving the file. Two names in one directory are never touched,
/// because an interrupted read and an interrupted (or running) unread look
/// the same; the next read or unread of the item resolves them.
pub fn recover(root: &Path) -> io::Result<Vec<PathBuf>> {
    let (_, extra) = resolve(collect(root, false)?);
    let mut removed = Vec::new();
    for (entry, kept) in extra {
        if entry.dir >= kept.dir {
            continue;
        }
        // Only unlink the name if both it and the kept name still point at
        // the same file.
        if still_names(&entry) && still_names(&kept) && remove_if_present(&entry.path)? {
            removed.push(entry.path);
        }
    }
    removed.sort();
    Ok(removed)
}

/// Whether `entry.path` still names the file it named when scanned.
fn still_names(entry: &Entry) -> bool {
    fs::symlink_metadata(&entry.path)
        .is_ok_and(|meta| meta.ino() == entry.inode && meta.dev() == entry.dev)
}

fn entry_at(dir: Dir, parsed: ParsedName, path: PathBuf) -> io::Result<Option<Entry>> {
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    Ok(Some(Entry {
        dir,
        unique: parsed.unique,
        flags: parsed.flags,
        path,
        inode: meta.ino(),
        mtime: meta.modified()?,
        len: meta.len(),
        dev: meta.dev(),
    }))
}

/// The well-formed names in one subdirectory, unstatted. A missing
/// subdirectory lists as empty.
fn list_names(root: &Path, dir: Dir) -> io::Result<Vec<(ParsedName, PathBuf)>> {
    let path = root.join(dir.name());
    let read = match fs::read_dir(&path) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for item in read {
        let item = item?;
        let Some(file_name) = item.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(parsed) = name::parse_name(&file_name) else {
            continue;
        };
        names.push((parsed, item.path()));
    }
    Ok(names)
}

/// Whether an archive unique is `<u>` or `<u>.<n>` for a known `<u>`.
fn archives_known(unique: &str, known: &HashSet<&str>) -> bool {
    if known.contains(unique) {
        return true;
    }
    unique.rsplit_once('.').is_some_and(|(base, number)| {
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && known.contains(base)
    })
}

/// Every entry in `new/` and `cur/`, plus the `archive/` entries whose
/// unique matches one of them (or all of `archive/` with `all_archive`).
fn collect(root: &Path, all_archive: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for dir in [Dir::New, Dir::Cur] {
        for (parsed, path) in list_names(root, dir)? {
            entries.extend(entry_at(dir, parsed, path)?);
        }
    }
    let known: HashSet<&str> = entries.iter().map(|e| e.unique.as_str()).collect();
    let mut archived = Vec::new();
    for (parsed, path) in list_names(root, Dir::Archive)? {
        if all_archive || archives_known(&parsed.unique, &known) {
            archived.extend(entry_at(Dir::Archive, parsed, path)?);
        }
    }
    entries.extend(archived);
    Ok(entries)
}

/// Splits entries into the names to keep (one per inode) and the extra names,
/// each paired with the name kept for its inode.
fn resolve(entries: Vec<Entry>) -> (Vec<Entry>, Vec<(Entry, Entry)>) {
    let mut groups: HashMap<(u64, u64), Vec<Entry>> = HashMap::new();
    for entry in entries {
        groups
            .entry((entry.dev, entry.inode))
            .or_default()
            .push(entry);
    }
    let mut keep = Vec::new();
    let mut extra = Vec::new();
    for (_, mut group) in groups {
        group.sort_by_key(Entry::keep_key);
        if let Some(kept) = group.pop() {
            extra.extend(group.into_iter().map(|entry| (entry, kept.clone())));
            keep.push(kept);
        }
    }
    (keep, extra)
}

/// The furthest-along `new/` or `cur/` name of the file `entry` names, found
/// by unique and inode, after another process (or a crash) moved it.
fn find_moved(root: &Path, entry: &Entry) -> io::Result<Option<Entry>> {
    let mut found = Vec::new();
    for dir in [Dir::New, Dir::Cur] {
        for (parsed, path) in list_names(root, dir)? {
            if parsed.unique != entry.unique {
                continue;
            }
            if let Some(candidate) = entry_at(dir, parsed, path)?
                && candidate.inode == entry.inode
                && candidate.dev == entry.dev
            {
                found.push(candidate);
            }
        }
    }
    Ok(found.into_iter().max_by_key(Entry::keep_key))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
        _ => false,
    }
}

/// Takes the advisory lock on the file `path` names, held until the returned
/// handle drops. Best effort: `None` when the file cannot be opened (read
/// only) or the file system has no locks.
fn lock_file(path: &Path) -> Option<File> {
    let file = File::open(path).ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Finishes an interrupted move whose destination `kept` already names the
/// file: unlinks `extra` only if, under the file's lock, both names still
/// point at one file. Returns whether the two still matched (and `extra` is
/// now gone).
fn unlink_twin(kept: &Path, extra: &Path) -> io::Result<bool> {
    let _lock = lock_file(kept);
    if !same_file(kept, extra) {
        return Ok(false);
    }
    remove_if_present(extra)?;
    Ok(true)
}

/// Removes `path`, treating an already missing name as done.
fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Marks an item read: `new/<u>` or `cur/<u>:2,<f>` becomes
/// `cur/<u>:2,<f>S`. Marking a read item read does nothing. Returns the
/// item's path afterwards.
pub fn mark_read(root: &Path, entry: &Entry) -> io::Result<PathBuf> {
    change_seen(root, entry, true)
}

/// Marks an item unread: `cur/<u>:2,<f>` loses `S` and stays in `cur/` (an
/// item never returns to `new/`). An item in `new/` is already unread.
pub fn mark_unread(root: &Path, entry: &Entry) -> io::Result<PathBuf> {
    change_seen(root, entry, false)
}

fn change_seen(root: &Path, entry: &Entry, seen: bool) -> io::Result<PathBuf> {
    if entry.dir == Dir::Archive {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an archived item has no read state to change",
        ));
    }
    let mut source = entry.clone();
    let mut retried = false;
    loop {
        let parsed = source.parsed_name();
        if source.is_read() == seen {
            if source.dir == Dir::Cur {
                // An interrupted read or unread may have left the other
                // state's name for the same file beside this one: drop it.
                let other = if seen {
                    parsed.with_flag_removed(SEEN)
                } else {
                    parsed.with_flag_added(SEEN)
                };
                let twin = root.join(CUR).join(other.render());
                if twin != source.path {
                    unlink_twin(&source.path, &twin)?;
                }
            }
            return Ok(source.path);
        }
        let target = if seen {
            parsed.with_flag_added(SEEN)
        } else {
            parsed.with_flag_removed(SEEN)
        };
        let destination = root.join(CUR).join(target.render());
        let error = match move_no_replace(&source.path, &destination) {
            Ok(()) => return Ok(destination),
            Err(error) => error,
        };
        let retry = match error.kind() {
            io::ErrorKind::AlreadyExists if same_file(&source.path, &destination) => {
                // An interrupted move left both names: finish the unlink,
                // unless another process changed them in the meantime.
                if unlink_twin(&destination, &source.path)? {
                    return Ok(destination);
                }
                true
            }
            io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound => true,
            _ => false,
        };
        if !retry || retried {
            return Err(error);
        }
        retried = true;
        match find_moved(root, &source)? {
            Some(moved) => source = moved,
            None => return Err(error),
        }
    }
}

/// Archives an item: `new/<u>` or `cur/<u>:2,<f>` becomes
/// `archive/<u>:2,<f>` with its flags kept. When that name is taken the
/// unique gets `.<n>` appended, with `n` counting up from 1 until a name is
/// free. Archiving an archived item does nothing. A stale `entry` whose file
/// was read or unread since the scan is found again by unique and inode.
pub fn archive(root: &Path, entry: &Entry) -> io::Result<PathBuf> {
    if entry.dir == Dir::Archive {
        return Ok(entry.path.clone());
    }
    match archive_once(root, entry) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => match find_moved(root, entry)? {
            Some(moved) => archive_once(root, &moved),
            None => Err(error),
        },
        result => result,
    }
}

fn archive_once(root: &Path, entry: &Entry) -> io::Result<PathBuf> {
    let flags = entry.flags.clone().unwrap_or_default();
    for n in 0..=MAX_ARCHIVE_SUFFIX {
        let unique = if n == 0 {
            entry.unique.clone()
        } else {
            format!("{}.{n}", entry.unique)
        };
        let destination = root.join(ARCHIVE).join(name::render(&unique, &flags));
        match move_no_replace(&entry.path, &destination) {
            Ok(()) => return Ok(destination),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if same_file(&entry.path, &destination) && unlink_twin(&destination, &entry.path)? {
                    return Ok(destination);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free archive name for {}", entry.unique),
    ))
}

/// Writes `item` to `tmp/<unique>`, syncs it, links it into `new/` and
/// unlinks the temporary name. Returns the path in `new/`. An existing file
/// is never replaced: a taken name is retried with a fresh unique.
pub fn deliver(root: &Path, item: &Item) -> io::Result<PathBuf> {
    let text = format::render(item);
    let mut last_error = None;
    for _ in 0..DELIVER_ATTEMPTS {
        let unique = name::unique_name_random(item.created, &item.source);
        let temp = root.join(TMP).join(&unique);
        let destination = root.join(NEW).join(&unique);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written {
            remove_if_present(&temp).ok();
            return Err(error);
        }
        match move_no_replace(&temp, &destination) {
            Ok(()) => {
                if let Ok(dir) = File::open(root.join(NEW)) {
                    dir.sync_all().ok();
                }
                return Ok(destination);
            }
            Err(error) => {
                remove_if_present(&temp).ok();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("no free unique name")))
}

static FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// Test seam: makes every hard link on this thread fail with EPERM.
    static LINK_UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn hard_link(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(test)]
    if LINK_UNSUPPORTED.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    fs::hard_link(source, destination)
}

/// Gives `source` the name `destination` and drops the old name, never
/// replacing an existing `destination` (that fails with `AlreadyExists`).
fn move_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    move_with(source, destination, hard_link)
}

fn link_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EPERM || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
        )
}

fn move_with(
    source: &Path,
    destination: &Path,
    link: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    // Held from the link to the unlink; see the module docs.
    let _lock = lock_file(source);
    match link(source, destination) {
        Ok(()) => {
            remove_if_present(source)?;
            Ok(())
        }
        Err(error) if link_unsupported(&error) => {
            // Some file systems (FAT, some network mounts) have no hard
            // links. Fall back to rename after checking the destination is
            // free; the small race window is the price of working there.
            if !FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "inbox: hard links unsupported under {} ({error}); using rename",
                    destination.parent().unwrap_or(destination).display()
                );
            }
            match fs::symlink_metadata(destination) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", destination.display()),
                )),
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    fs::rename(source, destination)
                }
                Err(other) => Err(other),
            }
        }
        Err(error) => Err(error),
    }
}

/// The parse result of one item file.
pub type ParseResult = Arc<Result<Item, Unreadable>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    inode: u64,
    mtime: SystemTime,
    len: u64,
}

/// Parse results cached by path, inode, mtime and length, so a rescan reads
/// only new or changed files.
#[derive(Debug, Default)]
pub struct ParseCache {
    parsed: HashMap<PathBuf, (CacheKey, ParseResult)>,
    reads: usize,
}

impl ParseCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// One result per entry, in order. A file that cannot be read or parsed
    /// gives an unreadable result. Cached results of paths not in `entries`
    /// are dropped. Read errors are not cached.
    pub fn parse(&mut self, entries: &[Entry]) -> Vec<ParseResult> {
        let mut next = HashMap::with_capacity(entries.len());
        let mut results = Vec::with_capacity(entries.len());
        for entry in entries {
            let key = CacheKey {
                inode: entry.inode,
                mtime: entry.mtime,
                len: entry.len,
            };
            let cached = self
                .parsed
                .remove(&entry.path)
                .filter(|(cached_key, _)| *cached_key == key);
            let result = match cached {
                Some((_, result)) => result,
                None => {
                    self.reads += 1;
                    match read_capped(&entry.path) {
                        Ok(bytes) => Arc::new(format::parse(&bytes)),
                        Err(error) => {
                            results.push(Arc::new(Err(Unreadable {
                                reason: format!("cannot read the file: {error}"),
                                line: None,
                            })));
                            continue;
                        }
                    }
                }
            };
            next.insert(entry.path.clone(), (key, result.clone()));
            results.push(result);
        }
        self.parsed = next;
        results
    }

    /// How many files this cache has read so far.
    pub fn reads(&self) -> usize {
        self.reads
    }
}

/// Reads at most one byte past [`format::MAX_FILE_BYTES`], which is enough for
/// [`format::parse`] to see the file is oversize and load only its header.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(format::MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
#[path = "maildir_tests.rs"]
mod tests;
