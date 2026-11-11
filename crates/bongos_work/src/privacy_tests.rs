//! The privacy promise of `task: start`: the path of a task's worktree is written into the task's
//! own file, the journals and git's own records, and into no file an agent reads. Each layout (the
//! central directory, a sibling directory and the fallback when the sibling is not writable)
//! starts a task and walks every file the start could have touched.
//!
//! The guard drives `task::Start` only, not the other commands. The real `HOME` is not redirected
//! in the tests, so it is not walked; only the temporary trees of the test are.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gpui::TestAppContext;

use crate::command_tests::Input;
use crate::worktree_tests::{Setup, central, git, set_repository_root, setup};

/// The files that tell an agent about a project, relative to a checkout.
const INSTRUCTION_FILES: [&str; 7] = [
    "CLAUDE.md",
    "CLAUDE.local.md",
    "AGENTS.md",
    "GEMINI.md",
    ".rules",
    ".cursorrules",
    ".github/copilot-instructions.md",
];

/// Names of directories an agent keeps its memory in.
const MEMORY_DIRECTORIES: [&str; 2] = ["memory", ".claude"];

/// The instruction files committed to the fixture repository, so the checked-out copies in the
/// worktree give the "unchanged" half of the check something to compare.
const COMMITTED: [(&str, &str); 2] = [
    ("CLAUDE.md", "Use tabs.\n"),
    ("AGENTS.md", "Run the tests.\n"),
];

/// What the test tree held before the start.
struct Before {
    /// Every instruction file of every walked root, by path.
    instructions: BTreeMap<PathBuf, Vec<u8>>,
    memory: BTreeSet<PathBuf>,
}

fn commit_instruction_files(setup: &Setup) {
    for (name, text) in COMMITTED {
        std::fs::write(setup.real.join(name), text).unwrap();
    }
    git(&setup.real, &["add", "CLAUDE.md", "AGENTS.md"]);
    git(&setup.real, &["commit", "-m", "add the instruction files"]);
}

/// Every directory of the walked trees: the harness's store and central directory, and the
/// repository's parent with the repository, its `.git` and any sibling directory.
fn roots(setup: &Setup) -> Vec<PathBuf> {
    vec![
        setup.harness.root.parent().unwrap().to_path_buf(),
        setup.real.parent().unwrap().to_path_buf(),
    ]
}

/// Visits every entry below `dir` without following symbolic links; an unreadable directory is
/// skipped.
fn walk(dir: &Path, visit: &mut impl FnMut(&Path, &std::fs::FileType)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = std::fs::symlink_metadata(&path).map(|meta| meta.file_type()) else {
            continue;
        };
        visit(&path, &kind);
        if kind.is_dir() {
            walk(&path, visit);
        }
    }
}

fn memory_directories(roots: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    for root in roots {
        walk(root, &mut |path, kind| {
            let named = path
                .file_name()
                .is_some_and(|name| MEMORY_DIRECTORIES.iter().any(|memory| name == *memory));
            if kind.is_dir() && named {
                found.insert(path.to_path_buf());
            }
        });
    }
    found
}

fn is_instruction_file(path: &Path) -> bool {
    INSTRUCTION_FILES
        .iter()
        .any(|name| path.ends_with(Path::new(name)))
}

/// The instruction files below the roots with their bytes, by path.
fn instruction_files(roots: &[PathBuf]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    for root in roots {
        walk(root, &mut |path, kind| {
            if kind.is_file()
                && is_instruction_file(path)
                && let Ok(bytes) = std::fs::read(path)
            {
                found.insert(path.to_path_buf(), bytes);
            }
        });
    }
    found
}

fn before(setup: &Setup) -> Before {
    let roots = roots(setup);
    Before {
        instructions: instruction_files(&roots),
        memory: memory_directories(&roots),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Whether `path` is a file bongos or git writes the worktree path into on purpose: a task file
/// or a journal in the store, and git's record of a linked worktree. The linked worktree's own
/// `.git` file does not hold the worktree path, so it is not allowed.
fn allowed(setup: &Setup, path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if path.starts_with(&setup.harness.root) {
        return name == "task.toml" || name.ends_with(".op.toml");
    }
    let parts: Vec<_> = path.components().rev().take(4).collect();
    name == "gitdir"
        && parts.len() == 4
        && parts[2].as_os_str() == "worktrees"
        && parts[3].as_os_str() == ".git"
}

/// Fails when any file but the allowed ones holds the path of `worktree`, when an instruction
/// file or a memory directory appeared, or when the walk saw no task file naming the worktree.
fn assert_no_path_leaks(setup: &Setup, worktree: &Path, before: &Before) {
    let canonical = std::fs::canonicalize(worktree).expect("the worktree exists");
    let needles: BTreeSet<Vec<u8>> = [worktree, canonical.as_path()]
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned().into_bytes())
        .collect();
    let roots = roots(setup);

    let mut leaks = Vec::new();
    let mut task_files_with_path = 0;
    for root in &roots {
        walk(root, &mut |path, kind| {
            if !kind.is_file() {
                return;
            }
            let Ok(bytes) = std::fs::read(path) else {
                return;
            };
            if !needles.iter().any(|needle| contains(&bytes, needle)) {
                return;
            }
            if allowed(setup, path) {
                if path.file_name().is_some_and(|name| name == "task.toml") {
                    task_files_with_path += 1;
                }
            } else {
                leaks.push(path.to_path_buf());
            }
        });
    }
    assert!(
        leaks.is_empty(),
        "files hold the worktree path {}: {leaks:?}",
        worktree.display()
    );
    assert!(
        task_files_with_path >= 1,
        "the walk found no task file with the worktree path, so it proves nothing"
    );

    // No walked root gained an instruction file or changed one, except the worktree's checked-out
    // copies, which must equal the repository's.
    let mut after = BTreeMap::new();
    let mut copies = BTreeMap::new();
    for (path, bytes) in instruction_files(&roots) {
        match path.strip_prefix(worktree) {
            Ok(relative) => copies.insert(relative.to_path_buf(), bytes),
            Err(_) => after.insert(path, bytes),
        };
    }
    assert_eq!(
        after, before.instructions,
        "an instruction file appeared or changed"
    );
    let committed: BTreeMap<PathBuf, Vec<u8>> = COMMITTED
        .iter()
        .map(|(name, text)| (PathBuf::from(name), text.as_bytes().to_vec()))
        .collect();
    assert_eq!(
        copies, committed,
        "the worktree's instruction files differ from the repository's"
    );
    assert_eq!(memory_directories(&roots), before.memory);
}

/// Restores the modes of the directories a test made read-only, also when it fails.
struct Restore(Vec<PathBuf>);

impl Drop for Restore {
    fn drop(&mut self) {
        for dir in &self.0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).ok();
        }
    }
}

#[gpui::test]
async fn a_central_worktree_path_stays_out_of_agent_files(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    commit_instruction_files(&setup);
    let before = before(&setup);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let worktree = central(harness, "backfill");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_no_path_leaks(&setup, &worktree, &before);
}

#[gpui::test]
async fn a_sibling_worktree_path_stays_out_of_agent_files(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    set_repository_root(cx, &setup.repository, None, Some("{repo}-worktrees"));
    commit_instruction_files(&setup);
    let before = before(&setup);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let worktree = setup
        .repository
        .parent()
        .unwrap()
        .join("invoicing-worktrees")
        .join("backfill");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_no_path_leaks(&setup, &worktree, &before);
}

#[gpui::test]
async fn a_fallback_worktree_path_stays_out_of_agent_files(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let sibling = setup
        .repository
        .parent()
        .unwrap()
        .join("invoicing-worktrees");
    std::fs::create_dir_all(&sibling).unwrap();
    let _restore = Restore(vec![sibling.clone()]);
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o555)).unwrap();
    if crate::worktree_root_tests::writable(&sibling) {
        eprintln!("skipped: the mode bits do not stop this user from writing");
        return;
    }
    set_repository_root(cx, &setup.repository, None, Some("{repo}-worktrees"));
    commit_instruction_files(&setup);
    let before = before(&setup);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let worktree = central(harness, "backfill");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_no_path_leaks(&setup, &worktree, &before);
}
