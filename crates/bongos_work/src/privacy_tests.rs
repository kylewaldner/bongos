//! The privacy promise of `task: start`: the path of a task's worktree is written into the task's
//! own file, the journals and git's own records, and into no file an agent reads. Each layout (the
//! central directory, a sibling directory and the fallback when the sibling is not writable)
//! starts a task and walks every file the start could have touched.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gpui::TestAppContext;

use crate::command_tests::Input;
use crate::worktree_tests::{Setup, central, set_repository_root, setup};

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

/// What the test tree held before the start.
struct Before {
    instructions: Vec<Option<Vec<u8>>>,
    memory: BTreeSet<PathBuf>,
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

fn instruction_files(checkout: &Path) -> Vec<Option<Vec<u8>>> {
    INSTRUCTION_FILES
        .iter()
        .map(|name| std::fs::read(checkout.join(name)).ok())
        .collect()
}

fn before(setup: &Setup) -> Before {
    Before {
        instructions: instruction_files(&setup.real),
        memory: memory_directories(&roots(setup)),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Whether `path` is a file bongos or git writes the worktree path into on purpose: a task file,
/// a journal, git's record of a linked worktree and the linked worktree's own `.git` file.
fn allowed(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name == "task.toml" || name.ends_with(".op.toml") || name == ".git" {
        return true;
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
            if allowed(path) {
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

    // Neither checkout gained an instruction file or changed one.
    assert_eq!(instruction_files(&setup.real), before.instructions);
    assert_eq!(instruction_files(worktree), before.instructions);
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
