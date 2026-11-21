//! The key a check result is stored under: the git tree id of a working copy's files, computed
//! without touching the real index, plus the helpers that fold several repositories and the
//! check's declared key inputs into one digest.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use bongos_project_repo::GitError;
use sha2::{Digest as _, Sha256};

use crate::task_git::TaskGit;
use crate::work_tree::{Problem, ensure_top_level};

/// What a missing key input file hashes as; no file's content digest can equal it.
const MISSING_INPUT: &str = "missing";

/// The key of one tree: the id of the git tree holding every tracked and every untracked,
/// not ignored file of a working copy, with their modes. Two working copies with the same files
/// have the same key, whatever commit they sit on. A combined key of several repositories
/// ([`combine_trees`]) is a 64-digit hex digest in the same type.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreeKey(pub String);

impl TreeKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("{path} is not a git working tree: {detail}")]
    NotGitTree { path: PathBuf, detail: String },
    #[error("{path} is inside the git working tree {top}, not at its top; pass the top directory")]
    NotTopLevel { path: PathBuf, top: PathBuf },
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("git returned {output:?} where a tree id was expected")]
    BadTreeId { output: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
}

impl TreeError {
    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// The tree key of the working copy at `tree`.
///
/// The files are staged into a private index file, `<index_dir>/<SHA-256 hex of the tree's
/// canonical path>`, through `GIT_INDEX_FILE`: `git add -A --ignore-errors .`, then
/// `git write-tree`. The repository's own index is never read or written, so its bytes and
/// modification time stay as they were. The private index is kept between calls, so git's stat
/// cache makes every call after the first cheap. A call only adds loose objects to the object
/// store; it moves no ref. A nested repository (a submodule, or a repository cloned inside) is
/// keyed as a gitlink to its `HEAD` commit, without running anything of its configuration: the
/// gitlinks are dropped from the private index before staging, so git stages each one afresh
/// from its `HEAD` instead of checking it with a `git status` inside it. Each call
/// works on its own copy of the private index and keeps it at the end, so calls for the same
/// tree at the same time do not disturb each other; the last one's index is kept.
///
/// Filter drivers are switched off, the global configuration's too (git-lfs's, for one), so in
/// a repository whose attributes select a filter the key is the tree of the files as they are on
/// disk, which may differ from the tree id of a commit of the same files.
///
/// A failure is an error, never a partial key. `--ignore-errors` only makes git stage every
/// file it can before it reports the files it could not (an unreadable file, one that vanished
/// while being read): git then exits non-zero and the call returns that as `TreeError::Git`,
/// with git's stderr. No such failure counts as ignorable, because a key that skips a file could
/// match a tree that differs in it.
///
/// Git runs through [`TaskGit`]: the tree's git directory is the one recorded for it, never the
/// `.git` the tree holds now, and no hook, filter or fsmonitor of the tree's configuration (or a
/// nested repository's) runs.
/// `tree` must be the top directory of the working tree; a subdirectory of one is
/// `TreeError::NotTopLevel`. The repository's `core.fileMode`, `core.fsmonitor`,
/// `core.trustctime`, `core.autocrlf` and `core.safecrlf` settings are overridden for the
/// calls, so the key does not depend on them, and the environment git inherits cannot redirect
/// it to another repository or index (see [`Git`]).
pub async fn tree_key(git: &TaskGit, tree: &Path, index_dir: &Path) -> Result<TreeKey, TreeError> {
    let canonical = tree.canonicalize().map_err(|error| TreeError::NotGitTree {
        path: tree.to_path_buf(),
        detail: error.to_string(),
    })?;
    ensure_top_level(git, &canonical)
        .await
        .map_err(|problem| match problem {
            Problem::NotWorkTree(detail) => TreeError::NotGitTree {
                path: tree.to_path_buf(),
                detail,
            },
            Problem::NotTopLevel(top) => TreeError::NotTopLevel {
                path: tree.to_path_buf(),
                top,
            },
        })?;

    std::fs::create_dir_all(index_dir)
        .map_err(|error| TreeError::io(format!("creating {}", index_dir.display()), error))?;
    let index_dir = std::path::absolute(index_dir)
        .map_err(|error| TreeError::io(format!("resolving {}", index_dir.display()), error))?;
    let index = private_index(&canonical, &index_dir);
    // This call's own copy of the kept index, so no other call can change it between the steps.
    let mut name = index.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}", uuid::Uuid::now_v7()));
    let work = index.with_file_name(name);
    if let Err(error) = copy_index(&index, &work) {
        std::fs::remove_file(&work).ok();
        return Err(TreeError::io(
            format!("copying the private index {}", index.display()),
            error,
        ));
    }
    let key = key_on_index(&git.allowing_nested_repositories(), &canonical, &work).await;
    let kept = match &key {
        Ok(_) => std::fs::rename(&work, &index).map_err(|error| {
            TreeError::io(
                format!("keeping the private index {}", index.display()),
                error,
            )
        }),
        Err(_) => Ok(()),
    };
    std::fs::remove_file(&work).ok();
    kept?;
    key
}

/// Copies the index file `index`, when there is one, to `work` with its modification time: git
/// tells a file changed within the same timestamp as the index was written (a racily clean
/// entry) by comparing with that time, so a copy with a later time would hide such an edit.
fn copy_index(index: &Path, work: &Path) -> std::io::Result<()> {
    let modified = match std::fs::metadata(index) {
        Ok(metadata) => metadata.modified()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    std::fs::copy(index, work)?;
    std::fs::File::options()
        .write(true)
        .open(work)?
        .set_modified(modified)
}

/// Stages the tree at `canonical` into the index file `index` and writes its tree. A nested
/// repository is staged as a gitlink from its `HEAD`. Git checks a gitlink already in the index
/// by running `git status` inside the nested repository, with its own configuration and filters,
/// so the index's gitlinks are dropped first: git then stages each nested repository afresh,
/// reading only its `HEAD`, and never runs anything of it.
async fn key_on_index(git: &TaskGit, canonical: &Path, index: &Path) -> Result<TreeKey, TreeError> {
    let listing = git
        .run_on_index(canonical, index, FIXED_SETTINGS, &["ls-files", "-s", "-z"])
        .await?;
    let mut gitlinks = Vec::new();
    for record in listing.split(|byte| *byte == 0) {
        // `<mode> <object> <stage>\t<path>`
        if let Some(path) = record.strip_prefix(b"160000 ")
            && let Some(tab) = path.iter().position(|byte| *byte == b'\t')
        {
            let path = std::str::from_utf8(&path[tab + 1..]).map_err(|_| {
                TreeError::io(
                    "listing the nested repositories of the private index",
                    std::io::Error::other("a nested repository's path is not UTF-8"),
                )
            })?;
            if !gitlinks.contains(&path) {
                gitlinks.push(path);
            }
        }
    }
    if !gitlinks.is_empty() {
        let mut args = vec!["update-index", "--force-remove", "--"];
        args.extend(gitlinks);
        git.run_on_index(canonical, index, FIXED_SETTINGS, &args)
            .await?;
    }
    git.run_on_index(
        canonical,
        index,
        FIXED_SETTINGS,
        &["add", "-A", "--ignore-errors", "."],
    )
    .await?;
    let output = git
        .run_on_index(canonical, index, FIXED_SETTINGS, &["write-tree"])
        .await?;
    let id = String::from_utf8_lossy(&output).trim().to_string();
    if id.len() < 40 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TreeError::BadTreeId { output: id });
    }
    Ok(TreeKey(id))
}

/// The settings that make the key the same whatever the repository's or the user's configuration
/// says: the executable bit and the change times are trusted, no file system monitor answers for
/// the stat cache, and no line ending is converted.
const FIXED_SETTINGS: &[(&str, &str)] = &[
    ("core.fileMode", "true"),
    ("core.fsmonitor", "false"),
    ("core.trustctime", "true"),
    ("core.autocrlf", "false"),
    ("core.safecrlf", "false"),
];

/// The private index [`tree_key`] keeps for the tree at `canonical` (its canonical path).
pub(crate) fn private_index(canonical: &Path, index_dir: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    index_dir.join(format!("{:x}", hasher.finalize()))
}

/// The key of a check's trees, one per repository: the tree's own key for one, else
/// [`combine_trees`] over the `(repository, tree key)` pairs.
pub(crate) async fn trees_key(
    git: &TaskGit,
    trees: &[(&str, &Path)],
    index_dir: &Path,
) -> Result<TreeKey, TreeError> {
    let mut keys = Vec::with_capacity(trees.len());
    for (repository, tree) in trees {
        keys.push((*repository, tree_key(git, tree, index_dir).await?));
    }
    match keys.as_slice() {
        [(_, key)] => Ok(key.clone()),
        _ => Ok(combine_trees(&keys)),
    }
}

/// One key for a check that spans several repositories: the SHA-256 hex of the
/// `(repository name, tree id)` pairs, sorted, so the order of `trees` does not matter. Each
/// part is length-prefixed, so no two different sets of pairs run together into the same bytes.
/// The key changes when any tree changes, and when a repository is added or removed.
pub fn combine_trees(trees: &[(&str, TreeKey)]) -> TreeKey {
    let mut pairs: Vec<(&str, &str)> = trees
        .iter()
        .map(|(repository, key)| (*repository, key.as_str()))
        .collect();
    pairs.sort_unstable();
    let mut hasher = Sha256::new();
    for (repository, tree) in pairs {
        update_part(&mut hasher, repository.as_bytes());
        update_part(&mut hasher, tree.as_bytes());
    }
    TreeKey(format!("{:x}", hasher.finalize()))
}

fn update_part(hasher: &mut Sha256, part: &[u8]) {
    hasher.update((part.len() as u64).to_be_bytes());
    hasher.update(part);
}

/// One digest of the files a check declares as key inputs (a lockfile, a toolchain pin): each
/// file's SHA-256 hex of its bytes, or the text `missing` when the file does not exist, joined
/// one per line in the order of `paths` and hashed again as SHA-256 hex. The order is kept, so
/// the same files in another order give another digest; the paths themselves are not part of the
/// digest, only their order and content. An existing file that cannot be read (permissions, a
/// directory) is an error, never a "missing".
pub fn key_inputs_digest(paths: &[PathBuf]) -> Result<String, TreeError> {
    let mut combined = Sha256::new();
    for path in paths {
        let digest = match std::fs::File::open(path) {
            Ok(file) => hash_file(file, path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MISSING_INPUT.to_string(),
            Err(error) => {
                return Err(TreeError::io(format!("opening {}", path.display()), error));
            }
        };
        combined.update(digest.as_bytes());
        combined.update(b"\n");
    }
    Ok(format!("{:x}", combined.finalize()))
}

/// [`key_inputs_digest`] of the files at `paths` inside the tree object `tree` of the repository
/// at `repository` instead of files on disk: the same digest when the tree holds the same bytes.
/// A path the tree does not hold is missing; one that is not a file there is an error.
pub(crate) async fn key_inputs_in_tree(
    git: &TaskGit,
    repository: &Path,
    tree: &str,
    paths: &[String],
) -> Result<String, TreeError> {
    if tree.len() < 40 || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TreeError::BadTreeId {
            output: tree.to_string(),
        });
    }
    let mut combined = Sha256::new();
    for path in paths {
        let path = path.trim_start_matches("./");
        let object = format!("{tree}:{path}");
        let digest = match git.run(repository, &["cat-file", "-t", &object]).await {
            Err(_) => MISSING_INPUT.to_string(),
            Ok(kind) if kind == "blob" => {
                let bytes = git
                    .run_raw(repository, &["cat-file", "blob", &object])
                    .await?;
                format!("{:x}", Sha256::digest(&bytes))
            }
            Ok(kind) => {
                return Err(TreeError::io(
                    format!("reading {path} in the tree {tree}"),
                    std::io::Error::other(format!("it is a {kind}, not a file")),
                ));
            }
        };
        combined.update(digest.as_bytes());
        combined.update(b"\n");
    }
    Ok(format!("{:x}", combined.finalize()))
}

fn hash_file(mut file: std::fs::File, path: &Path) -> Result<String, TreeError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| TreeError::io(format!("reading {}", path.display()), error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
