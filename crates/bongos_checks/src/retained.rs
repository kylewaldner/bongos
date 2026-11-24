//! The refs that keep the trees of finished tasks under groups with closing checks:
//! `refs/bongos/checks/<task id>` pointing at the tree object a tree key names, or, for a key of
//! several repositories, `refs/bongos/checks/<task id>/<repository>` in each. A ref keeps its tree
//! from git's garbage collection after the worktree it was computed in is removed, so the closing
//! checks of the group can still run on it.
//!
//! The refs live in the code repository whose object store received the tree (a linked worktree
//! writes them into its common git directory), never in the definitions repository, and nothing
//! pushes them. A task id or repository name that could not be a plain ref component (a `/`,
//! `..`, a leading `.` or `-`, a `.lock` ending, anything outside letters, digits, `.`, `_` and
//! `-`) gets no ref.

use std::path::Path;

use crate::task_git::TaskGit;
use anyhow::{Result, anyhow};

/// Where the refs live.
pub(crate) const CHECKS_REFS: &str = "refs/bongos/checks/";

/// Whether `part` can be one ref component as it is.
pub(crate) fn safe_component(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 200
        && !part.starts_with('.')
        && !part.starts_with('-')
        && !part.ends_with(".lock")
        && !part.contains("..")
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The ref of `task_id`'s tree, or of its tree in `repo` for a key of several repositories.
pub(crate) fn ref_name(task_id: &str, repo: Option<&str>) -> Option<String> {
    if !safe_component(task_id) {
        return None;
    }
    match repo {
        None => Some(format!("{CHECKS_REFS}{task_id}")),
        Some(repo) if safe_component(repo) => Some(format!("{CHECKS_REFS}{task_id}/{repo}")),
        Some(_) => None,
    }
}

fn ensure_object_id(id: &str) -> Result<()> {
    if id.len() >= 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(anyhow!("{id:?} is not an object id"))
    }
}

/// One tree to keep: the repository's name, a directory of that repository (a worktree or the
/// primary checkout) and the tree id.
pub(crate) struct KeptTree<'a> {
    pub(crate) repo: &'a str,
    pub(crate) directory: &'a Path,
    pub(crate) tree: &'a str,
}

/// Points the refs of `task_id` at `trees`: one tree is `refs/bongos/checks/<task id>`, several
/// are one ref per repository. The other form is deleted first, so the two never collide.
pub(crate) async fn keep_trees(git: &TaskGit, task_id: &str, trees: &[KeptTree<'_>]) -> Result<()> {
    let single = ref_name(task_id, None).ok_or_else(|| anyhow!("{task_id:?} cannot name a ref"))?;
    match trees {
        [] => Ok(()),
        [tree] => {
            ensure_object_id(tree.tree)?;
            for (name, _) in list_refs(git, tree.directory).await? {
                if name.starts_with(&format!("{single}/")) {
                    git.run(tree.directory, &["update-ref", "-d", &name])
                        .await?;
                }
            }
            git.run(tree.directory, &["update-ref", &single, tree.tree])
                .await?;
            Ok(())
        }
        _ => {
            for tree in trees {
                ensure_object_id(tree.tree)?;
                let name = ref_name(task_id, Some(tree.repo))
                    .ok_or_else(|| anyhow!("{:?} cannot name a ref", tree.repo))?;
                git.run(tree.directory, &["update-ref", "-d", &single])
                    .await?;
                git.run(tree.directory, &["update-ref", &name, tree.tree])
                    .await?;
            }
            Ok(())
        }
    }
}

/// The tree a ref of `task_id` names in the repository at `directory`, if it names one.
pub(crate) async fn kept_tree(
    git: &TaskGit,
    directory: &Path,
    task_id: &str,
    repo: Option<&str>,
) -> Option<String> {
    let name = ref_name(task_id, repo)?;
    git.run(
        directory,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{tree}}"),
        ],
    )
    .await
    .ok()
    .filter(|id| ensure_object_id(id).is_ok())
}

/// Every ref under `refs/bongos/checks/` of the repository at `directory`, with the task id it
/// names.
pub(crate) async fn list_refs(git: &TaskGit, directory: &Path) -> Result<Vec<(String, String)>> {
    let output = git
        .run(
            directory,
            &["for-each-ref", "--format=%(refname)", CHECKS_REFS],
        )
        .await?;
    Ok(output
        .lines()
        .filter_map(|name| {
            let rest = name.strip_prefix(CHECKS_REFS)?;
            let task = rest.split('/').next()?;
            Some((name.to_string(), task.to_string()))
        })
        .collect())
}

/// Deletes the ref `name`, which must be under `refs/bongos/checks/`.
pub(crate) async fn delete_ref(git: &TaskGit, directory: &Path, name: &str) -> Result<()> {
    if !name.starts_with(CHECKS_REFS) {
        return Err(anyhow!("{name} is not a ref of task checks"));
    }
    git.run(directory, &["update-ref", "-d", name]).await?;
    Ok(())
}

/// Whether the repository at `directory` holds the tree object `tree`.
pub(crate) async fn has_tree(git: &TaskGit, directory: &Path, tree: &str) -> bool {
    ensure_object_id(tree).is_ok()
        && git
            .run(directory, &["cat-file", "-t", tree])
            .await
            .is_ok_and(|kind| kind == "tree")
}
