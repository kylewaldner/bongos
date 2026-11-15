//! The key a check result is stored under: the git tree id of a working copy's files, computed
//! without touching the real index, plus the helpers that fold several repositories and the
//! check's declared key inputs into one digest.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use bongos_project_repo::{Git, GitError};
use sha2::{Digest as _, Sha256};

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
/// store; it moves no ref. A nested repository is recorded as one entry (its HEAD commit), not
/// by its files. Calls for the same tree at the same time contend for the private index's lock
/// and the loser gets an error; the caller may retry.
///
/// A failure is an error, never a partial key. `--ignore-errors` only makes git stage every
/// file it can before it reports the files it could not (an unreadable file, one that vanished
/// while being read): git then exits non-zero and the call returns that as `TreeError::Git`,
/// with git's stderr. No such failure counts as ignorable, because a key that skips a file could
/// match a tree that differs in it.
pub async fn tree_key(git: &Git, tree: &Path, index_dir: &Path) -> Result<TreeKey, TreeError> {
    let canonical = tree.canonicalize().map_err(|error| TreeError::NotGitTree {
        path: tree.to_path_buf(),
        detail: error.to_string(),
    })?;
    ensure_work_tree(git, &canonical, tree).await?;

    std::fs::create_dir_all(index_dir)
        .map_err(|error| TreeError::io(format!("creating {}", index_dir.display()), error))?;
    let index_dir = std::path::absolute(index_dir)
        .map_err(|error| TreeError::io(format!("resolving {}", index_dir.display()), error))?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    let index = index_dir.join(format!("{:x}", hasher.finalize()));

    git.run_on_index(
        &canonical,
        &index,
        &["add", "-A", "--ignore-errors", "."],
        None,
    )
    .await?;
    let output = git
        .run_on_index(&canonical, &index, &["write-tree"], None)
        .await?;
    let id = String::from_utf8_lossy(&output).trim().to_string();
    if id.len() < 40 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TreeError::BadTreeId { output: id });
    }
    Ok(TreeKey(id))
}

/// Fails unless `directory` is inside a git working tree.
async fn ensure_work_tree(git: &Git, directory: &Path, shown: &Path) -> Result<(), TreeError> {
    let not_a_tree = |detail: String| TreeError::NotGitTree {
        path: shown.to_path_buf(),
        detail,
    };
    match git
        .run(directory, &["rev-parse", "--is-inside-work-tree"])
        .await
    {
        Ok(answer) if answer == "true" => Ok(()),
        Ok(_) => Err(not_a_tree("not inside a working tree".to_string())),
        Err(GitError::Failed { stderr, .. }) => Err(not_a_tree(stderr)),
        Err(error) => Err(error.into()),
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
