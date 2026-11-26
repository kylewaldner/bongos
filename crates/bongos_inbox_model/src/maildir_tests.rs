use super::*;
use chrono::{TimeZone as _, Utc};
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

fn item(summary: &str) -> Item {
    Item {
        version: 1,
        kind: "note".into(),
        created: Utc.with_ymd_and_hms(2026, 11, 11, 3, 12, 45).unwrap(),
        source: "agent/test".into(),
        group: None,
        target: None,
        summary: summary.into(),
        actions: vec!["accept".into()],
        extra: Vec::new(),
        body: format!("Body of {summary}.\n"),
        body_loaded: true,
    }
}

fn setup() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("inbox");
    ensure_layout(&root).unwrap();
    (temp, root)
}

fn put(root: &Path, dir: &str, file_name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(dir).join(file_name);
    fs::write(&path, bytes).unwrap();
    path
}

fn put_item(root: &Path, dir: &str, file_name: &str, summary: &str) -> PathBuf {
    put(
        root,
        dir,
        file_name,
        format::render(&item(summary)).as_bytes(),
    )
}

/// Every name under the four subdirectories, as `dir/name`, sorted.
fn names(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in [TMP, NEW, CUR, ARCHIVE] {
        for entry in fs::read_dir(root.join(dir)).unwrap() {
            let entry = entry.unwrap();
            out.push(format!("{dir}/{}", entry.file_name().to_str().unwrap()));
        }
    }
    out.sort();
    out
}

fn all(root: &Path) -> Vec<Entry> {
    scan_with(
        root,
        ScanOptions {
            include_archive: true,
        },
    )
    .unwrap()
}

fn only(root: &Path) -> Entry {
    let mut entries = all(root);
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.remove(0)
}

fn inode(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}

fn fixed_time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn set_mtime(path: &Path, time: SystemTime) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

#[test]
fn ensure_layout_creates_private_dirs() {
    let (_temp, root) = setup();
    ensure_layout(&root).unwrap();
    for dir in [TMP, NEW, CUR, ARCHIVE] {
        let mode = fs::metadata(root.join(dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{dir}");
    }
}

#[test]
fn deliver_lands_in_new_and_leaves_tmp_empty() {
    let (_temp, root) = setup();
    let path = deliver(&root, &item("one")).unwrap();
    assert_eq!(path.parent().unwrap(), root.join(NEW));
    let file_name = path.file_name().unwrap().to_str().unwrap();
    assert!(file_name.starts_with("20261111T031245Z-agent-test-"));
    assert_eq!(names(&root), vec![format!("new/{file_name}")]);
    let entry = only(&root);
    assert_eq!(entry.dir, Dir::New);
    assert!(!entry.is_read());
    let parsed = ParseCache::new().parse(&[entry]);
    assert_eq!(parsed[0].as_ref().as_ref().unwrap(), &item("one"));
}

#[test]
fn deliver_never_reuses_a_name() {
    let (_temp, root) = setup();
    let first = deliver(&root, &item("a")).unwrap();
    let second = deliver(&root, &item("a")).unwrap();
    assert_ne!(first, second);
    assert_eq!(scan(&root).unwrap().len(), 2);
}

#[test]
fn tmp_is_never_scanned() {
    let (_temp, root) = setup();
    put_item(&root, TMP, "a.md", "a");
    put_item(&root, TMP, "b.md:2,S", "b");
    assert!(all(&root).is_empty());
    assert!(recover(&root).unwrap().is_empty());
}

#[test]
fn partial_write_in_tmp_then_rename_appears_whole() {
    let (_temp, root) = setup();
    let text = format::render(&item("partial"));
    let (head, tail) = text.as_bytes().split_at(text.len() / 2);
    let temp_path = put(&root, TMP, "p.md", head);
    assert!(all(&root).is_empty());
    File::options()
        .append(true)
        .open(&temp_path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    fs::rename(&temp_path, root.join(NEW).join("p.md")).unwrap();
    let entry = only(&root);
    assert_eq!(entry.len, text.len() as u64);
    let parsed = ParseCache::new().parse(&[entry]);
    assert_eq!(parsed[0].as_ref().as_ref().unwrap(), &item("partial"));
}

#[test]
fn read_unread_and_archive_keep_bytes_and_inode() {
    let (_temp, root) = setup();
    let delivered = deliver(&root, &item("keep")).unwrap();
    let bytes = fs::read(&delivered).unwrap();
    let ino = inode(&delivered);
    let unique = only(&root).unique;

    let read = mark_read(&root, &only(&root)).unwrap();
    assert_eq!(read, root.join(CUR).join(format!("{unique}:2,S")));
    assert_eq!(names(&root), vec![format!("cur/{unique}:2,S")]);
    assert!(only(&root).is_read());

    let unread = mark_unread(&root, &only(&root)).unwrap();
    assert_eq!(unread, root.join(CUR).join(format!("{unique}:2,")));
    assert_eq!(names(&root), vec![format!("cur/{unique}:2,")]);
    assert!(!only(&root).is_read());

    let read_again = mark_read(&root, &only(&root)).unwrap();
    let archived = archive(&root, &only(&root)).unwrap();
    assert_eq!(archived, root.join(ARCHIVE).join(format!("{unique}:2,S")));
    assert_eq!(names(&root), vec![format!("archive/{unique}:2,S")]);
    assert!(scan(&root).unwrap().is_empty());

    for path in [&read, &unread, &read_again] {
        assert!(!path.exists() || *path == archived);
    }
    assert_eq!(fs::read(&archived).unwrap(), bytes);
    assert_eq!(inode(&archived), ino);
}

#[test]
fn flags_other_than_seen_are_kept() {
    let (_temp, root) = setup();
    put_item(&root, CUR, "x.md:2,FT", "x");
    let read = mark_read(&root, &only(&root)).unwrap();
    assert_eq!(read.file_name().unwrap(), "x.md:2,FST");
    let archived = archive(&root, &only(&root)).unwrap();
    assert_eq!(archived.file_name().unwrap(), "x.md:2,FST");
}

#[test]
fn no_operation_opens_a_file_for_writing() {
    let (_temp, root) = setup();
    let path = deliver(&root, &item("untouched")).unwrap();
    let bytes = fs::read(&path).unwrap();
    let time = fixed_time(1_700_000_000);
    set_mtime(&path, time);
    // Read-only, so any open for writing by a non-root user would fail.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    let check = |path: &Path| {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(
            fs::symlink_metadata(path).unwrap().modified().unwrap(),
            time
        );
    };
    check(&mark_read(&root, &only(&root)).unwrap());
    check(&mark_unread(&root, &only(&root)).unwrap());
    check(&archive(&root, &only(&root)).unwrap());
}

#[test]
fn repeating_an_operation_is_a_no_op() {
    let (_temp, root) = setup();
    deliver(&root, &item("twice")).unwrap();
    let fresh = only(&root);
    assert_eq!(mark_unread(&root, &fresh).unwrap(), fresh.path);
    let read = mark_read(&root, &fresh).unwrap();
    assert_eq!(mark_read(&root, &only(&root)).unwrap(), read);
    let archived = archive(&root, &only(&root)).unwrap();
    assert_eq!(archive(&root, &only(&root)).unwrap(), archived);
    assert_eq!(names(&root).len(), 1);
    assert_eq!(
        mark_read(&root, &only(&root)).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn a_stale_entry_finds_the_moved_file() {
    let (_temp, root) = setup();
    deliver(&root, &item("stale")).unwrap();
    let stale = only(&root);
    let read = mark_read(&root, &stale).unwrap();
    // Another process already marked it read: the stale new/ name is gone.
    assert_eq!(mark_read(&root, &stale).unwrap(), read);
    let stale_read = only(&root);
    let unread = mark_unread(&root, &stale_read).unwrap();
    assert_eq!(mark_unread(&root, &stale_read).unwrap(), unread);
    assert_eq!(names(&root).len(), 1);
}

#[test]
fn archiving_a_stale_entry_finds_the_moved_file() {
    let (_temp, root) = setup();
    deliver(&root, &item("stale archive")).unwrap();
    let stale = only(&root);
    let bytes = fs::read(&stale.path).unwrap();
    // Another process marked it read: the stale new/ name is gone.
    mark_read(&root, &stale).unwrap();
    let archived = archive(&root, &stale).unwrap();
    assert_eq!(
        archived,
        root.join(ARCHIVE).join(format!("{}:2,S", stale.unique))
    );
    assert_eq!(names(&root), vec![format!("archive/{}:2,S", stale.unique)]);
    assert_eq!(fs::read(&archived).unwrap(), bytes);
}

#[test]
fn an_existing_read_destination_is_never_replaced() {
    let (_temp, root) = setup();
    let source = put(&root, NEW, "u.md", b"source bytes");
    let taken = put(&root, CUR, "u.md:2,S", b"other file");
    let entry = scan(&root)
        .unwrap()
        .into_iter()
        .find(|entry| entry.dir == Dir::New)
        .unwrap();
    let error = mark_read(&root, &entry).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&source).unwrap(), b"source bytes");
    assert_eq!(fs::read(&taken).unwrap(), b"other file");
}

#[test]
fn an_archive_collision_gets_a_numbered_name() {
    let (_temp, root) = setup();
    let first = put(&root, ARCHIVE, "u.md:2,S", b"first");
    put(&root, CUR, "u.md:2,S", b"second");
    let second = archive(&root, &only_in(&root, Dir::Cur)).unwrap();
    assert_eq!(second, root.join(ARCHIVE).join("u.md.1:2,S"));
    put(&root, NEW, "u.md:2,S", b"third");
    let third = archive(&root, &only_in(&root, Dir::New)).unwrap();
    assert_eq!(third, root.join(ARCHIVE).join("u.md.2:2,S"));
    assert_eq!(fs::read(&first).unwrap(), b"first");
    assert_eq!(fs::read(&second).unwrap(), b"second");
    assert_eq!(fs::read(&third).unwrap(), b"third");
    let mut uniques: Vec<String> = all(&root).into_iter().map(|e| e.unique).collect();
    uniques.sort();
    assert_eq!(uniques, vec!["u.md", "u.md.1", "u.md.2"]);
}

fn only_in(root: &Path, dir: Dir) -> Entry {
    let mut entries: Vec<Entry> = all(root).into_iter().filter(|e| e.dir == dir).collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.remove(0)
}

/// Leaves `from` and its hard link `to`, as a crash between link and unlink would.
fn crash(root: &Path, from: (&str, &str), to: (&str, &str)) -> (PathBuf, PathBuf) {
    let source = put_item(root, from.0, from.1, "crashed");
    let destination = root.join(to.0).join(to.1);
    fs::hard_link(&source, &destination).unwrap();
    (source, destination)
}

fn assert_recovers(root: &Path, source: &Path, kept: &Path) {
    let bytes = fs::read(kept).unwrap();
    let rows: Vec<PathBuf> = all(root).into_iter().map(|e| e.path).collect();
    assert!(rows.contains(&kept.to_path_buf()), "{rows:?}");
    assert!(!rows.contains(&source.to_path_buf()), "{rows:?}");
    assert_eq!(recover(root).unwrap(), vec![source.to_path_buf()]);
    assert!(!source.exists());
    assert_eq!(fs::read(kept).unwrap(), bytes);
    assert!(recover(root).unwrap().is_empty());
}

#[test]
fn a_crash_during_read_recovers_to_one_name() {
    let (_temp, root) = setup();
    let (source, destination) = crash(&root, (NEW, "u.md"), (CUR, "u.md:2,S"));
    assert_eq!(scan(&root).unwrap().len(), 1);
    assert_recovers(&root, &source, &destination);
    assert_eq!(names(&root), vec!["cur/u.md:2,S"]);
}

/// A same-directory pair: one row (the read name), no name unlinked by
/// `recover`, bytes intact.
fn assert_pair_kept(root: &Path, read: &Path, unread: &Path) {
    let bytes = fs::read(read).unwrap();
    let rows: Vec<PathBuf> = all(root).into_iter().map(|e| e.path).collect();
    assert_eq!(rows, vec![read.to_path_buf()]);
    assert!(recover(root).unwrap().is_empty());
    assert_eq!(fs::read(read).unwrap(), bytes);
    assert_eq!(fs::read(unread).unwrap(), bytes);
}

/// A crash during a read (`2,` to `2,S`) or an unread (`2,S` to `2,`) in
/// `cur/` leaves both names; the next read or unread converges to one.
#[test]
fn a_crash_during_read_or_unread_in_cur_keeps_both_names_until_the_next_change() {
    let orders = [("u.md:2,", "u.md:2,S"), ("u.md:2,S", "u.md:2,")];
    let changes = [(true, "cur/u.md:2,S"), (false, "cur/u.md:2,")];
    for (from, to) in orders {
        for (seen, kept) in changes {
            let (_temp, root) = setup();
            crash(&root, (CUR, from), (CUR, to));
            let read = root.join(CUR).join("u.md:2,S");
            let unread = root.join(CUR).join("u.md:2,");
            assert_pair_kept(&root, &read, &unread);
            let bytes = fs::read(&read).unwrap();
            let path = if seen {
                mark_read(&root, &only(&root)).unwrap()
            } else {
                mark_unread(&root, &only(&root)).unwrap()
            };
            assert_eq!(names(&root), vec![kept], "{from} -> {to}, seen {seen}");
            assert_eq!(path, root.join(kept));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}

#[test]
fn recover_during_an_unread_never_drops_the_last_name() {
    let (_temp, root) = setup();
    let read = put_item(&root, CUR, "u.md:2,S", "racing");
    let unread = root.join(CUR).join("u.md:2,");
    let bytes = fs::read(&read).unwrap();
    // The unread as `mark_unread` does it, with `recover` run by "another
    // process" between the link and the unlink.
    move_with(&read, &unread, |source, destination| {
        fs::hard_link(source, destination)?;
        assert!(recover(&root).unwrap().is_empty());
        assert!(source.exists() && destination.exists());
        Ok(())
    })
    .unwrap();
    assert_eq!(names(&root), vec!["cur/u.md:2,"]);
    assert_eq!(fs::read(&unread).unwrap(), bytes);
    assert!(recover(&root).unwrap().is_empty());
    assert_eq!(names(&root), vec!["cur/u.md:2,"]);
}

#[test]
fn a_crash_during_archive_recovers_to_one_name() {
    let (_temp, root) = setup();
    let (source, destination) = crash(&root, (CUR, "u.md:2,FS"), (ARCHIVE, "u.md:2,FS"));
    assert!(scan(&root).unwrap().is_empty());
    assert_recovers(&root, &source, &destination);
    assert_eq!(names(&root), vec!["archive/u.md:2,FS"]);
}

#[test]
fn a_crash_during_a_numbered_archive_recovers_to_one_name() {
    let (_temp, root) = setup();
    put(&root, ARCHIVE, "u.md:2,", b"older item");
    let (source, destination) = crash(&root, (NEW, "u.md"), (ARCHIVE, "u.md.1:2,"));
    assert!(scan(&root).unwrap().is_empty());
    assert_recovers(&root, &source, &destination);
    assert_eq!(names(&root), vec!["archive/u.md.1:2,", "archive/u.md:2,"]);
    assert_eq!(
        fs::read(root.join(ARCHIVE).join("u.md:2,")).unwrap(),
        b"older item"
    );
}

#[test]
fn repeating_an_interrupted_operation_finishes_it() {
    let (_temp, root) = setup();
    let stale = |unique: &str, path: &Path| {
        let meta = fs::metadata(path).unwrap();
        Entry {
            dir: Dir::New,
            unique: unique.into(),
            flags: None,
            path: path.to_path_buf(),
            inode: meta.ino(),
            mtime: meta.modified().unwrap(),
            len: meta.len(),
            dev: meta.dev(),
        }
    };
    let (source, destination) = crash(&root, (NEW, "u.md"), (CUR, "u.md:2,S"));
    assert_eq!(
        mark_read(&root, &stale("u.md", &source)).unwrap(),
        destination
    );
    assert_eq!(names(&root), vec!["cur/u.md:2,S"]);

    let (source, destination) = crash(&root, (NEW, "v.md"), (ARCHIVE, "v.md:2,"));
    assert_eq!(
        archive(&root, &stale("v.md", &source)).unwrap(),
        destination
    );
    assert!(!source.exists());
}

#[test]
fn a_fresh_scan_reads_state_from_names_alone() {
    let (_temp, root) = setup();
    put(&root, NEW, "a.md", b"not an item");
    put(&root, NEW, "b.md:2,S", b"not an item");
    put(&root, CUR, "c.md:2,S", b"not an item");
    put(&root, CUR, "d.md:2,F", b"not an item");
    put(&root, CUR, "e.md:2,FS", b"not an item");
    let state = |entries: Vec<Entry>| -> Vec<(String, bool)> {
        entries
            .into_iter()
            .map(|e| (e.unique.clone(), e.is_read()))
            .collect()
    };
    let expected = vec![
        ("c.md".to_string(), true),
        ("d.md".to_string(), false),
        ("e.md".to_string(), true),
        ("a.md".to_string(), false),
        ("b.md".to_string(), false),
    ];
    assert_eq!(state(scan(&root).unwrap()), expected);
    assert_eq!(state(scan(&root).unwrap()), expected);
}

#[test]
fn malformed_names_dot_files_dirs_and_links_are_skipped() {
    let (_temp, root) = setup();
    let good = put_item(&root, CUR, "good.md:2,S", "good");
    put_item(&root, NEW, ".hidden.md", "x");
    put_item(&root, CUR, ".hidden.md:2,S", "x");
    put_item(&root, CUR, "x.md:1,S", "x");
    put_item(&root, CUR, "y.md:2,S1", "x");
    put_item(&root, CUR, "z.md:", "x");
    put_item(&root, ARCHIVE, "w.md:2", "x");
    fs::create_dir(root.join(NEW).join("sub.md")).unwrap();
    fs::create_dir(root.join(CUR).join("dir.md:2,S")).unwrap();
    std::os::unix::fs::symlink(&good, root.join(CUR).join("link.md:2,S")).unwrap();
    let rows: Vec<PathBuf> = all(&root).into_iter().map(|e| e.path).collect();
    assert_eq!(rows, vec![good]);
}

#[test]
fn an_unreadable_file_is_a_row_beside_its_siblings() {
    let (_temp, root) = setup();
    put_item(&root, NEW, "a.md", "a");
    put(&root, NEW, "b.md", b"garbage\n");
    put(&root, NEW, "c.md", b"");
    put_item(&root, NEW, "d.md", "d");
    let entries = scan(&root).unwrap();
    assert_eq!(entries.len(), 4);
    let parsed = ParseCache::new().parse(&entries);
    assert_eq!(parsed[0].as_ref().as_ref().unwrap().summary, "a");
    assert!(parsed[1].is_err());
    assert!(parsed[2].is_err());
    assert_eq!(parsed[3].as_ref().as_ref().unwrap().summary, "d");
}

#[test]
fn the_cache_reparses_only_changed_files() {
    let (_temp, root) = setup();
    for (file_name, time) in [("a.md", 1), ("b.md", 2), ("c.md", 3)] {
        let path = put_item(&root, NEW, file_name, file_name);
        set_mtime(&path, fixed_time(1_700_000_000 + time));
    }
    let mut cache = ParseCache::new();
    cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 3);
    cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 3);

    // Same length, new content and an explicitly different mtime.
    let b = root.join(NEW).join("b.md");
    fs::write(&b, format::render(&item("B.md"))).unwrap();
    set_mtime(&b, fixed_time(1_800_000_000));
    let parsed = cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 4);
    assert_eq!(parsed[1].as_ref().as_ref().unwrap().summary, "B.md");

    // A new name is a new path.
    let entries = scan(&root).unwrap();
    mark_read(&root, &entries[0]).unwrap();
    cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 5);
    cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 5);
}

#[test]
fn without_hard_links_moves_fall_back_to_rename() {
    let (_temp, root) = setup();
    let errors: [fn() -> io::Error; 3] = [
        || io::Error::from_raw_os_error(libc::EPERM),
        || io::Error::from_raw_os_error(libc::ENOTSUP),
        || io::Error::from(io::ErrorKind::Unsupported),
    ];
    for error in errors {
        let source = put(&root, NEW, "f.md", b"bytes");
        let ino = inode(&source);
        let destination = root.join(CUR).join("f.md:2,S");
        move_with(&source, &destination, |_, _| Err(error())).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"bytes");
        assert_eq!(inode(&destination), ino);

        // A taken destination is still never replaced.
        let source = put(&root, NEW, "f.md", b"other");
        let result = move_with(&source, &destination, |_, _| Err(error()));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"other");
        assert_eq!(fs::read(&destination).unwrap(), b"bytes");
        fs::remove_file(&source).unwrap();
        fs::remove_file(&destination).unwrap();
    }

    // Any other link error is returned and nothing moves.
    let source = put(&root, NEW, "g.md", b"bytes");
    let destination = root.join(CUR).join("g.md:2,S");
    let result = move_with(&source, &destination, |_, _| {
        Err(io::Error::from_raw_os_error(libc::EACCES))
    });
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EACCES));
    assert!(source.exists());
    assert!(!destination.exists());
}

#[test]
fn operations_work_where_hard_links_are_unsupported() {
    let (_temp, root) = setup();
    LINK_UNSUPPORTED.with(|flag| flag.set(true));
    let delivered = deliver(&root, &item("fat")).unwrap();
    let bytes = fs::read(&delivered).unwrap();
    let read = mark_read(&root, &only(&root)).unwrap();
    assert!(only(&root).is_read());
    put(
        &root,
        ARCHIVE,
        &read.file_name().unwrap().to_string_lossy(),
        b"taken",
    );
    let archived = archive(&root, &only_in(&root, Dir::Cur)).unwrap();
    assert!(
        archived
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".1:2,S")
    );
    assert_eq!(fs::read(&archived).unwrap(), bytes);
    assert_eq!(names(&root).len(), 2);
    LINK_UNSUPPORTED.with(|flag| flag.set(false));
}

fn fill(root: &Path, count: usize) {
    let text = format::render(&item("many"));
    for i in 0..count {
        let (dir, file_name) = if i % 2 == 0 {
            (NEW, format!("item-{i:04}.md"))
        } else {
            (CUR, format!("item-{i:04}.md:2,S"))
        };
        put(root, dir, &file_name, text.as_bytes());
    }
}

#[test]
fn a_scan_of_2000_items_lists_them_all() {
    let (_temp, root) = setup();
    fill(&root, 2000);
    let entries = scan(&root).unwrap();
    assert_eq!(entries.len(), 2000);
    assert_eq!(entries.iter().filter(|e| e.is_read()).count(), 1000);
    let mut cache = ParseCache::new();
    assert!(cache.parse(&entries).iter().all(|r| r.is_ok()));
    cache.parse(&scan(&root).unwrap());
    assert_eq!(cache.reads(), 2000);
}

mod perf_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn scanning_and_parsing_2000_items_stays_within_budget() {
        let (_temp, root) = setup();
        fill(&root, 2000);
        let mut cache = ParseCache::new();
        let start = Instant::now();
        let entries = scan(&root).unwrap();
        cache.parse(&entries);
        let first = start.elapsed();
        let start = Instant::now();
        cache.parse(&scan(&root).unwrap());
        let rescan = start.elapsed();
        assert_eq!(entries.len(), 2000);
        assert!(first < Duration::from_secs(3), "first scan took {first:?}");
        assert!(rescan < Duration::from_secs(2), "rescan took {rescan:?}");
    }
}
