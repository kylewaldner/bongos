//! [`PrivateGit::import`]; see the documentation of the parent module.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::Output,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{
    PrivateGit, TreeLock, canonical, line_bytes, remove_marker, remove_no_follow, restore_dot_git,
};
use crate::safe_git::{SafeGit, UNTRUSTED_OBJECTS_CONFIG, read_regular_file_limited};

/// The import directory's name in the linked git directory.
pub(super) const IMPORT_DIR: &str = "bongos-import";

/// The largest `HEAD` or loose ref file read.
const REF_FILE_LIMIT: u64 = 4 << 10;

/// The largest `packed-refs` read.
const PACKED_REFS_LIMIT: u64 = 64 << 20;

/// The largest `objects/info/alternates` read.
const ALTERNATES_LIMIT: u64 = 64 << 10;

/// The deepest directory below `refs/` read; deeper entries are refused.
const REFS_DEPTH: usize = 32;

/// The most entries read below `refs/`, and the most lines read from `packed-refs`.
const REFS_LIMIT: usize = 100_000;

/// The most entries read below the private `objects/`.
const OBJECTS_LIMIT: usize = 1_000_000;

/// The reflog message of an import.
const REFLOG_MESSAGE: &str = "bongos: import a confined job's commits";

/// What [`PrivateGit::import`] did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportReport {
    /// The refs written to the shared repository, each a full ref name.
    pub imported: Vec<String>,
    /// Each entry of the private directory that was not imported, with the reason, as a line
    /// (`not imported: refs/tags/v1 (only commits and branches travel back)`).
    pub refused: Vec<String>,
    /// What the job left that changes nothing in the shared repository (`HEAD was on topic; the
    /// tree is back on feature`), and anything about switching the tree back.
    pub notes: Vec<String>,
    /// The private directory, kept until the next preparation because something was refused.
    pub kept_private_dir: Option<PathBuf>,
}

impl PrivateGit {
    /// Imports the job's work into the shared repository and switches the tree back (see the
    /// module documentation). Call it only once every process of the job is gone. `git` is the
    /// host git for this tree's linked git directory; its binary, environment and configuration
    /// are used for every command. Fails, importing nothing, when the private object directory is
    /// refused, packing or the connectivity check fails, or a ref moved in the shared repository;
    /// the tree is switched back all the same and the private directory kept.
    pub async fn import(self, git: &SafeGit) -> Result<ImportReport> {
        self.import_with(git, &util::job_roots::contains).await
    }

    /// [`PrivateGit::import`] with `in_job_root` deciding what lies inside a job root.
    pub(crate) async fn import_with(
        self,
        git: &SafeGit,
        in_job_root: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<ImportReport> {
        let git_dir = canonical(git.git_dir(), "git directory")?;
        if git_dir != self.linked_git_dir {
            bail!(
                "importing into {} needs git for {}",
                git_dir.display(),
                self.linked_git_dir.display()
            );
        }
        // Held to the end, so no preparation or recovery of the tree runs during the import.
        let _lock = TreeLock::take(&self.linked_git_dir)?;
        self.check_marker()?;

        let shared = git.for_git_dir(&self.linked_git_dir, None);
        let import_dir = self.linked_git_dir.join(IMPORT_DIR);
        let transferred = transfer(&self, &shared, &import_dir, in_job_root).await;
        self.finish(&shared, &import_dir, transferred, in_job_root)
            .await
    }

    /// Switches the tree back after the transfer: restores `.git`, resets the shared index to the
    /// own branch when the import moved it, removes the import directory, removes the private
    /// directory unless something was refused or the transfer failed, then removes the marker.
    async fn finish(
        &self,
        shared: &SafeGit,
        import_dir: &Path,
        transferred: Result<Transferred>,
        in_job_root: &(dyn Fn(&Path) -> bool + Sync),
    ) -> Result<ImportReport> {
        if let Err(error) = remove_no_follow(import_dir) {
            log::error!("private git: {error:#}");
        }
        if let Err(restore_error) = restore_dot_git(&self.tree, &self.original_dot_git) {
            let what = match &transferred {
                Ok(transferred) if !transferred.report.imported.is_empty() => format!(
                    "imported {}, but switching the tree back failed",
                    transferred.report.imported.join(", ")
                ),
                Ok(_) => "switching the tree back failed".to_string(),
                Err(error) => {
                    format!("the import failed ({error:#}), and switching the tree back failed too")
                }
            };
            return Err(restore_error.context(format!(
                "{what}; the marker in {} is left for recovery",
                self.linked_git_dir.display()
            )));
        }
        let keep = match &transferred {
            Ok(transferred) => !transferred.report.refused.is_empty(),
            Err(_) => true,
        };
        let mut report = match transferred {
            Ok(Transferred {
                mut report,
                own_branch_moved_to,
            }) => {
                if let Some(commit) = own_branch_moved_to {
                    self.reset_index(shared, &commit, &mut report, in_job_root)
                        .await;
                }
                Ok(report)
            }
            Err(error) => Err(error.context(format!(
                "nothing was imported; the private directory {} is kept until the next preparation",
                self.private_dir.display()
            ))),
        };
        if keep {
            if let Ok(report) = &mut report {
                report.kept_private_dir = Some(self.private_dir.clone());
            }
        } else {
            remove_no_follow(&self.private_dir)?;
        }
        remove_marker(&self.linked_git_dir)?;
        report
    }

    /// Makes the shared index match `commit`, the own branch's new tip, keeping the working
    /// files: `reset -q` in the tree, or, when that fails (host git refuses a tree holding a
    /// nested repository, for one), `read-tree` without the work tree, which leaves every
    /// entry's file data to be refreshed by the next `git status`.
    async fn reset_index(
        &self,
        shared: &SafeGit,
        commit: &str,
        report: &mut ImportReport,
        in_job_root: &(dyn Fn(&Path) -> bool + Sync),
    ) {
        let tree_git = shared.for_git_dir(&self.linked_git_dir, Some(self.tree.clone()));
        let reset = run(&tree_git, &["reset", "-q", "--mixed"], None, in_job_root).await;
        let Err(reset_error) = reset else {
            return;
        };
        let read_tree = run(shared, &["read-tree", commit], None, in_job_root).await;
        match read_tree {
            Ok(_) => report.notes.push(format!(
                "the index was reset without the work tree ({reset_error:#}); the next git status \
                 rereads every file"
            )),
            Err(error) => {
                log::error!(
                    "private git: cannot reset the index of {}: {error:#}",
                    self.tree.display()
                );
                report.notes.push(format!(
                    "the index of the tree could not be reset ({reset_error:#}; {error:#}); \
                     run git reset in the tree"
                ));
            }
        }
    }
}

/// The transfer's outcome: the report so far and the own branch's new tip when it was imported.
struct Transferred {
    report: ImportReport,
    own_branch_moved_to: Option<String>,
}

/// A ref the import writes: `old` is the snapshot value for the own branch, `None` for a new
/// branch.
struct Update {
    name: String,
    new: String,
    old: Option<String>,
}

/// Steps 1 to 4 of the import (see the module documentation): reads the private refs, compares
/// them with the snapshot, moves the new objects and writes the allowed refs in one transaction.
async fn transfer(
    private: &PrivateGit,
    shared: &SafeGit,
    import_dir: &Path,
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<Transferred> {
    let hex_len = private
        .snapshot
        .get(&private.branch)
        .map(String::len)
        .ok_or_else(|| anyhow!("the snapshot lacks the branch {}", private.branch))?;
    let refs = read_private_refs(&private.private_dir, hex_len)?;
    let mut report = ImportReport::default();
    report.refused.extend(refs.refused.iter().cloned());
    let mut updates = compare(private, &refs, &mut report);

    let objects = private.private_dir.join("objects");
    let import = shared
        .for_git_dir(import_dir, None)
        .job_object_directory(&objects);
    if !updates.is_empty() {
        let shared_objects = private.common_dir.join("objects");
        let mut expected = line_bytes(&shared_objects, "shared object directory")?.to_vec();
        expected.push(b'\n');
        check_private_objects(&objects, hex_len, &expected)?;

        make_import_dir(
            shared,
            import_dir,
            &objects,
            &shared_objects,
            hex_len,
            in_job_root,
        )
        .await?;
        keep_commits(&import, &mut updates, &mut report, in_job_root).await?;
    }
    if !updates.is_empty() {
        let mut revs = String::new();
        for update in &updates {
            revs.push_str(&format!("{}\n", update.new));
        }
        revs.push_str("--not\n");
        for oid in private.snapshot.values().collect::<BTreeSet<_>>() {
            revs.push_str(&format!("{oid}\n"));
        }
        move_objects(&import, shared, revs.as_bytes(), in_job_root).await?;

        let mut checked = shared.clone();
        for (key, value) in UNTRUSTED_OBJECTS_CONFIG {
            checked = checked.config(*key, *value);
        }
        run(
            &checked,
            &[
                "rev-list",
                "--objects",
                "--missing=error",
                "--quiet",
                "--stdin",
            ],
            Some(revs.as_bytes()),
            in_job_root,
        )
        .await
        .context("the imported commits are not complete in the shared repository")?;

        write_refs(shared, &updates, in_job_root).await?;
    }

    let mut own_branch_moved_to = None;
    for update in &updates {
        report.imported.push(update.name.clone());
        if update.name == private.branch {
            own_branch_moved_to = Some(update.new.clone());
        }
    }
    check_detached_head(
        private,
        shared,
        &refs.head,
        &updates,
        &mut report,
        in_job_root,
    )
    .await?;
    Ok(Transferred {
        report,
        own_branch_moved_to,
    })
}

/// Sorts the private refs against the snapshot: returns the allowed updates and adds a line to
/// `report` for each refused entry and for `HEAD`.
fn compare(private: &PrivateGit, refs: &PrivateRefs, report: &mut ImportReport) -> Vec<Update> {
    let own = &private.branch;
    let mut updates = Vec::new();
    for (name, new) in &refs.values {
        let is_branch = name.starts_with("refs/heads/");
        match private.snapshot.get(name) {
            Some(old) if old == new => {}
            Some(old) if name == own => updates.push(Update {
                name: name.clone(),
                new: new.clone(),
                old: Some(old.clone()),
            }),
            Some(_) if is_branch => report.refused.push(format!(
                "not imported: {name} (a branch of the shared repository other than {} moved)",
                short(own)
            )),
            None if is_branch => updates.push(Update {
                name: name.clone(),
                new: new.clone(),
                old: None,
            }),
            _ => report.refused.push(format!(
                "not imported: {name} (only commits and branches travel back)"
            )),
        }
    }
    for name in private.snapshot.keys() {
        if refs.values.contains_key(name) || refs.refused_names.contains(name) {
            continue;
        }
        if name == own {
            report.refused.push(format!(
                "not deleted: {name} (the job deleted its own branch; the shared one is unchanged)"
            ));
        } else {
            report.refused.push(format!(
                "not deleted: {name} (the job deleted a ref of the shared repository)"
            ));
        }
    }
    match &refs.head {
        Head::Branch(branch) if branch == own => {}
        Head::Branch(branch) => report.notes.push(format!(
            "HEAD was on {}; the tree is back on {}",
            short(branch),
            short(own)
        )),
        Head::Detached(_) => {}
        Head::Refused(reason) => report
            .refused
            .push(format!("not imported: HEAD ({reason})")),
    }
    updates
}

/// Refuses each update whose new value is not a commit, read in the import directory.
async fn keep_commits(
    import: &SafeGit,
    updates: &mut Vec<Update>,
    report: &mut ImportReport,
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<()> {
    let mut input = String::new();
    for update in updates.iter() {
        input.push_str(&format!("{}\n", update.new));
    }
    let output = run(
        import,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(input.as_bytes()),
        in_job_root,
    )
    .await?;
    let text = String::from_utf8(output.stdout).context("unexpected cat-file output")?;
    let types: Vec<&str> = text.lines().collect();
    if types.len() != updates.len() {
        bail!("unexpected cat-file output {text:?}");
    }
    let mut kept = Vec::new();
    for (update, line) in updates.drain(..).zip(types) {
        match line.split_once(' ') {
            Some((oid, "commit")) if oid == update.new => kept.push(update),
            Some((oid, kind)) if oid == update.new && kind != "missing" => {
                report.refused.push(format!(
                    "not imported: {} (it names a {kind}, not a commit)",
                    update.name
                ))
            }
            _ if line == format!("{} missing", update.new) => report.refused.push(format!(
                "not imported: {} (it names an object that does not exist)",
                update.name
            )),
            _ => bail!("unexpected cat-file output {line:?}"),
        }
    }
    *updates = kept;
    Ok(())
}

/// Makes the import directory afresh: a bare repository of the shared object format whose
/// alternates are the private, then the shared object directory.
async fn make_import_dir(
    shared: &SafeGit,
    import_dir: &Path,
    private_objects: &Path,
    shared_objects: &Path,
    hex_len: usize,
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<()> {
    remove_no_follow(import_dir)?;
    fs::create_dir(import_dir).with_context(|| format!("creating {}", import_dir.display()))?;
    let format = if hex_len == 64 { "sha256" } else { "sha1" };
    run(
        &shared.for_git_dir(import_dir, None),
        &[
            "init",
            "--bare",
            "-q",
            "--template=",
            &format!("--object-format={format}"),
        ],
        None,
        in_job_root,
    )
    .await?;
    let mut alternates = line_bytes(private_objects, "private object directory")?.to_vec();
    alternates.push(b'\n');
    alternates.extend_from_slice(line_bytes(shared_objects, "shared object directory")?);
    alternates.push(b'\n');
    super::write_new(&import_dir.join("objects/info/alternates"), &alternates)
}

/// Streams `pack-objects --revs --thin` of `revs` in the import directory into `index-pack
/// --stdin --fix-thin --strict` in the shared repository. An empty pack (every object is
/// already shared) is not indexed.
async fn move_objects(
    import: &SafeGit,
    shared: &SafeGit,
    revs: &[u8],
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<()> {
    use util::command::Stdio;

    let mut pack_command = import
        .command_with(
            &["pack-objects", "--revs", "--thin", "--stdout", "-q"],
            in_job_root,
        )
        .await?;
    pack_command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut index_command = shared
        .command_with(
            &["index-pack", "--stdin", "--fix-thin", "--strict"],
            in_job_root,
        )
        .await?;
    index_command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut pack = pack_command.spawn().context("starting git pack-objects")?;
    let mut pack_input = pack.stdin.take();
    let mut pack_output = pack
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git pack-objects has no output pipe"))?;
    let feed = async move {
        if let Some(pipe) = pack_input.as_mut() {
            pipe.write_all(revs).await?;
            pipe.close().await?;
        }
        drop(pack_input);
        Ok::<(), io::Error>(())
    };
    // Owns the pack's output pipe, so it closes when this ends and a pack-objects still writing
    // stops on the broken pipe instead of blocking.
    let forward = async move {
        let mut header = [0u8; 12];
        pack_output
            .read_exact(&mut header)
            .await
            .context("reading the pack header")?;
        let [b'P', b'A', b'C', b'K', _, _, _, _, c0, c1, c2, c3] = header else {
            bail!("git pack-objects wrote no pack");
        };
        if u32::from_be_bytes([c0, c1, c2, c3]) == 0 {
            futures::io::copy(&mut pack_output, &mut futures::io::sink())
                .await
                .context("reading the pack")?;
            return Ok(None);
        }
        let mut index = index_command.spawn().context("starting git index-pack")?;
        let mut index_input = index
            .stdin
            .take()
            .ok_or_else(|| anyhow!("git index-pack has no input pipe"))?;
        let copy = async move {
            index_input.write_all(&header).await?;
            futures::io::copy(&mut pack_output, &mut index_input).await?;
            index_input.close().await?;
            drop(index_input);
            Ok::<(), io::Error>(())
        };
        let (copied, indexed) = futures::join!(copy, index.output());
        Ok(Some((copied, indexed.context("running git index-pack")?)))
    };
    let (fed, forwarded, packed) = futures::join!(feed, forward, pack.output());
    let packed = packed.context("running git pack-objects")?;
    if !packed.status.success() {
        bail!(
            "git pack-objects failed: {}",
            String::from_utf8_lossy(&packed.stderr).trim()
        );
    }
    fed.context("writing to git pack-objects")?;
    if let Some((copied, indexed)) = forwarded? {
        if !indexed.status.success() {
            bail!(
                "git index-pack failed: {}",
                String::from_utf8_lossy(&indexed.stderr).trim()
            );
        }
        copied.context("writing to git index-pack")?;
    }
    Ok(())
}

/// Writes `updates` to the shared repository in one `update-ref` transaction: the own branch
/// moves only from its snapshot value, a new branch is created only where none exists.
async fn write_refs(
    shared: &SafeGit,
    updates: &[Update],
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<()> {
    let mut input = String::new();
    for update in updates {
        match &update.old {
            Some(old) => input.push_str(&format!("update {} {} {old}\n", update.name, update.new)),
            None => input.push_str(&format!("create {} {}\n", update.name, update.new)),
        }
    }
    let Err(error) = run(
        shared,
        &["update-ref", "-m", REFLOG_MESSAGE, "--stdin"],
        Some(input.as_bytes()),
        in_job_root,
    )
    .await
    else {
        return Ok(());
    };
    // Name the ref that moved, when one did; otherwise git's own message (a stale lock, for one)
    // says why.
    let output = run(
        shared,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads/",
        ],
        None,
        in_job_root,
    )
    .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let current: BTreeMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(oid, name)| (name, oid)))
        .collect();
    for update in updates {
        let now = current.get(update.name.as_str()).copied();
        if now != update.old.as_deref() {
            if update.old.is_some() {
                bail!(
                    "branch {} moved while the job ran; nothing imported",
                    short(&update.name)
                );
            }
            bail!(
                "branch {} was created in the shared repository while the job ran; nothing imported",
                short(&update.name)
            );
        }
    }
    Err(error.context("writing the imported refs failed; nothing imported"))
}

/// Refuses a detached `HEAD` at a commit that no imported ref and no snapshot ref reaches,
/// asked of the shared repository after the import; notes one that is reached.
async fn check_detached_head(
    private: &PrivateGit,
    shared: &SafeGit,
    head: &Head,
    updates: &[Update],
    report: &mut ImportReport,
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<()> {
    let Head::Detached(oid) = head else {
        return Ok(());
    };
    let tips: BTreeSet<&str> = updates
        .iter()
        .map(|update| update.new.as_str())
        .chain(private.snapshot.values().map(String::as_str))
        .collect();
    let mut reached = tips.contains(oid.as_str());
    if !reached {
        let mut input = format!("{oid}\n--not\n");
        for tip in &tips {
            input.push_str(&format!("{tip}\n"));
        }
        // A commit the shared repository lacks fails rev-list: not reached.
        reached = run(
            shared,
            &["rev-list", "-n", "1", "--stdin"],
            Some(input.as_bytes()),
            in_job_root,
        )
        .await
        .is_ok_and(|output| output.stdout.is_empty());
    }
    let short_oid = oid.get(..12).unwrap_or(oid);
    if reached {
        report.notes.push(format!(
            "HEAD was detached at {short_oid}; the tree is back on {}",
            short(&private.branch)
        ));
    } else {
        report.refused.push(format!(
            "HEAD detached at {short_oid}: not on a branch, not imported"
        ));
    }
    Ok(())
}

/// Runs `args` through `git`, writing `input` to its standard input; an exit status other than
/// 0 fails with git's message.
async fn run(
    git: &SafeGit,
    args: &[&str],
    input: Option<&[u8]>,
    in_job_root: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<Output> {
    use util::command::Stdio;

    let describe = || format!("git {}", args.first().copied().unwrap_or_default());
    let mut command = git
        .command_with(args, in_job_root)
        .await
        .with_context(describe)?;
    let output = match input {
        None => command.output().await.with_context(describe)?,
        Some(input) => {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().with_context(describe)?;
            let mut pipe = child.stdin.take();
            // Written while the output is read, so neither pipe can stall the other.
            let feed = async move {
                if let Some(pipe) = pipe.as_mut() {
                    pipe.write_all(input).await?;
                    pipe.close().await?;
                }
                drop(pipe);
                Ok::<(), io::Error>(())
            };
            let (fed, output) = futures::join!(feed, child.output());
            let output = output.with_context(describe)?;
            if output.status.success() {
                fed.with_context(describe)?;
            }
            output
        }
    };
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            describe(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// What the private `HEAD` holds.
#[derive(Debug, Eq, PartialEq)]
enum Head {
    Branch(String),
    Detached(String),
    Refused(String),
}

/// The private refs, read as data.
struct PrivateRefs {
    /// Every readable ref with its object id; a loose ref overrides a packed one.
    values: BTreeMap<String, String>,
    /// The names among the refused entries, so a refused ref is not also reported deleted.
    refused_names: BTreeSet<String>,
    refused: Vec<String>,
    head: Head,
}

/// What a ref file holds.
enum RefValue {
    Object(String),
    Symbolic(String),
}

/// Reads `HEAD`, the loose refs under `refs/` and `packed-refs` of the private directory with
/// bongos's own parser, never git: each file is opened without following a symlink or blocking,
/// and bounded; each name must pass [`check_ref_format`] and each value must be a full object
/// id of `hex_len` digits. An entry that fails is refused; a directory or file that cannot be
/// read at all fails the import.
fn read_private_refs(private_dir: &Path, hex_len: usize) -> Result<PrivateRefs> {
    real_dir(private_dir)?;
    let head = match read_ref_file(&private_dir.join("HEAD"), REF_FILE_LIMIT) {
        Err(reason) => Head::Refused(reason),
        Ok(None) => Head::Refused("it is missing".to_string()),
        Ok(Some(bytes)) => match parse_ref_value(&bytes, hex_len) {
            Err(reason) => Head::Refused(reason),
            Ok(RefValue::Object(oid)) => Head::Detached(oid),
            Ok(RefValue::Symbolic(name))
                if name.starts_with("refs/heads/") && check_ref_format(&name) =>
            {
                Head::Branch(name)
            }
            Ok(RefValue::Symbolic(name)) => {
                Head::Refused(format!("it names {name:?}, not a branch"))
            }
        },
    };
    let mut refs = PrivateRefs {
        values: BTreeMap::new(),
        refused_names: BTreeSet::new(),
        refused: Vec::new(),
        head,
    };

    let mut loose = BTreeMap::new();
    let refs_dir = private_dir.join("refs");
    let mut pending = match fs::symlink_metadata(&refs_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", refs_dir.display()));
        }
        Ok(metadata) if metadata.is_dir() => vec![(refs_dir, "refs".to_string(), 1usize)],
        Ok(_) => bail!("{} is not a directory", refs_dir.display()),
    };
    let mut seen = 0usize;
    while let Some((dir, prefix, depth)) = pending.pop() {
        let reading = || format!("reading {}", dir.display());
        for entry in fs::read_dir(&dir).with_context(reading)? {
            let entry = entry.with_context(reading)?;
            seen += 1;
            if seen > REFS_LIMIT {
                bail!("the private refs hold more than {REFS_LIMIT} entries");
            }
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                let name = format!("{prefix}/{}", file_name.to_string_lossy());
                refuse_entry(&mut refs, &name, "a name that is not UTF-8");
                continue;
            };
            let name = format!("{prefix}/{file_name}");
            let file_type = entry.file_type().with_context(reading)?;
            if file_type.is_dir() {
                if depth >= REFS_DEPTH {
                    refuse_entry(&mut refs, &name, "nested too deep");
                } else {
                    pending.push((entry.path(), name, depth + 1));
                }
                continue;
            }
            if !file_type.is_file() {
                refuse_entry(
                    &mut refs,
                    &name,
                    "a symlink or special file, not a ref file",
                );
                continue;
            }
            if !check_ref_format(&name) {
                refuse_entry(&mut refs, &name, "not a valid ref name");
                continue;
            }
            match read_ref_file(&entry.path(), REF_FILE_LIMIT) {
                Err(reason) => refuse_entry(&mut refs, &name, &reason),
                Ok(None) => {}
                Ok(Some(bytes)) => match parse_ref_value(&bytes, hex_len) {
                    Ok(RefValue::Object(oid)) => {
                        loose.insert(name, oid);
                    }
                    Ok(RefValue::Symbolic(_)) => refuse_entry(&mut refs, &name, "a symbolic ref"),
                    Err(reason) => refuse_entry(&mut refs, &name, &reason),
                },
            }
        }
    }

    let mut packed = BTreeMap::new();
    let packed_refs = private_dir.join("packed-refs");
    let bytes = read_ref_file(&packed_refs, PACKED_REFS_LIMIT)
        .map_err(|reason| anyhow!("cannot read {}: {reason}", packed_refs.display()))?
        .unwrap_or_default();
    let mut previous_is_ref = false;
    let mut duplicates = BTreeSet::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if index >= REFS_LIMIT {
            bail!(
                "{} holds more than {REFS_LIMIT} lines",
                packed_refs.display()
            );
        }
        let number = index + 1;
        let bad_line = |refs: &mut PrivateRefs, reason: &str| {
            refs.refused.push(format!(
                "not imported: packed-refs line {number} ({reason})"
            ));
        };
        if line.is_empty() {
            previous_is_ref = false;
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            bad_line(&mut refs, "not UTF-8");
            previous_is_ref = false;
            continue;
        };
        if index == 0 && line.starts_with("# pack-refs with:") {
            continue;
        }
        if let Some(peeled) = line.strip_prefix('^') {
            if !previous_is_ref || !valid_object_id(peeled, hex_len) {
                bad_line(&mut refs, "an unexpected peeled line");
            }
            previous_is_ref = false;
            continue;
        }
        previous_is_ref = false;
        let Some((oid, name)) = line.split_once(' ') else {
            bad_line(&mut refs, "not a ref line");
            continue;
        };
        if !check_ref_format(name) {
            refuse_entry(&mut refs, name, "not a valid ref name");
            continue;
        }
        if !valid_object_id(oid, hex_len) {
            refuse_entry(&mut refs, name, "not a full object name");
            continue;
        }
        if packed.insert(name.to_string(), oid.to_string()).is_some() {
            duplicates.insert(name.to_string());
        }
        previous_is_ref = true;
    }
    for name in duplicates {
        packed.remove(&name);
        refuse_entry(&mut refs, &name, "listed twice in packed-refs");
    }

    for (name, oid) in packed {
        if !loose.contains_key(&name) && !refs.refused_names.contains(&name) {
            loose.insert(name, oid);
        }
    }
    // A name refused anywhere is refused whole: a refused loose file never falls back to its
    // packed value.
    loose.retain(|name, _| !refs.refused_names.contains(name));
    refs.values = loose;
    Ok(refs)
}

/// The contents of the regular file `path`, `None` when nothing is there; a reason when it is
/// anything else, cannot be read or is larger than `limit`. Opened without following a symlink
/// and without blocking (a FIFO would block a plain open), then checked through the handle.
fn read_ref_file(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read as _;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let not_regular = || "a symlink or special file, not a ref file".to_string();
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) => {
            return Err(not_regular());
        }
        Err(error) => return Err(format!("unreadable: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("unreadable: {error}"))?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    let too_large = || format!("larger than {limit} bytes");
    if metadata.len() > limit {
        return Err(too_large());
    }
    let mut contents = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut contents)
        .map_err(|error| format!("unreadable: {error}"))?;
    if contents.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(Some(contents))
}

/// Records the entry `name` as refused for `reason`.
fn refuse_entry(refs: &mut PrivateRefs, name: &str, reason: &str) {
    refs.refused_names.insert(name.to_string());
    refs.refused
        .push(format!("not imported: {} ({reason})", printable(name)));
}

/// `name` as written, or quoted with escapes when it holds a control character.
fn printable(name: &str) -> String {
    if name.chars().any(char::is_control) {
        format!("{name:?}")
    } else {
        name.to_string()
    }
}

/// Parses a ref file: a full object id or `ref: <name>`, each with one optional newline.
fn parse_ref_value(bytes: &[u8], hex_len: usize) -> Result<RefValue, String> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())?;
    if let Some(name) = text.strip_prefix("ref: ") {
        return Ok(RefValue::Symbolic(name.to_string()));
    }
    if valid_object_id(text, hex_len) {
        Ok(RefValue::Object(text.to_string()))
    } else {
        Err("not a full object name".to_string())
    }
}

/// A full lowercase hexadecimal object id of `hex_len` digits, not the null id.
fn valid_object_id(oid: &str, hex_len: usize) -> bool {
    oid.len() == hex_len
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && oid.bytes().any(|byte| byte != b'0')
}

/// Whether `name` is a full ref name under `refs/` that `git check-ref-format` accepts: no
/// control character, space, `~`, `^`, `:`, `?`, `*`, `[` or `\`; no `..` or `@{`; no empty
/// component, none starting with `.` or ending with `.lock`; not ending with `.` or `/`.
pub(crate) fn check_ref_format(name: &str) -> bool {
    let forbidden = |byte: u8| byte < 0x20 || byte == 0x7f || b" ~^:?*[\\".contains(&byte);
    name.starts_with("refs/")
        && !name.bytes().any(forbidden)
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with('.')
        && name.split('/').count() >= 2
        && name.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        })
}

/// Refuses the private object directory unless it holds only what git writes for loose objects
/// and plain packs, plus the alternates prepare wrote (`expected_alternates`): loose objects in
/// two-digit directories, `pack/pack-<id>.pack` and `.idx`, and `info/alternates`. A symlink, a
/// changed or missing alternates, a commit graph, a multi-pack index, bitmaps, reverse indexes,
/// keep or promisor files and anything else are refused, since git would read them as trusted.
fn check_private_objects(objects: &Path, hex_len: usize, expected_alternates: &[u8]) -> Result<()> {
    let refuse = |path: &Path, reason: &str| {
        anyhow!(
            "refusing the private object directory: {} {reason}",
            path.display()
        )
    };
    real_dir(objects)?;
    let is_hex = |text: &str, len: usize| {
        text.len() == len
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let mut seen = 0usize;
    let mut entries = |dir: &Path| -> Result<Vec<(String, PathBuf, fs::FileType)>> {
        let reading = || format!("reading {}", dir.display());
        let mut list = Vec::new();
        for entry in fs::read_dir(dir).with_context(reading)? {
            let entry = entry.with_context(reading)?;
            seen += 1;
            if seen > OBJECTS_LIMIT {
                bail!("the private object directory holds more than {OBJECTS_LIMIT} entries");
            }
            let path = entry.path();
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| refuse(&path, "has a name that is not UTF-8"))?;
            list.push((name, path, entry.file_type().with_context(reading)?));
        }
        Ok(list)
    };
    let mut alternates_found = false;
    for (name, path, file_type) in entries(objects)? {
        if !file_type.is_dir() {
            return Err(refuse(&path, "is not a git object directory entry"));
        }
        if name == "info" {
            for (name, path, file_type) in entries(&path)? {
                if name != "alternates" || !file_type.is_file() {
                    return Err(refuse(&path, "is not allowed"));
                }
                let contents = read_regular_file_limited(&path, ALTERNATES_LIMIT)?;
                if contents.as_deref() != Some(expected_alternates) {
                    return Err(refuse(&path, "was changed"));
                }
                alternates_found = true;
            }
        } else if name == "pack" {
            for (name, path, file_type) in entries(&path)? {
                let id = name
                    .strip_prefix("pack-")
                    .and_then(|rest| {
                        rest.strip_suffix(".pack")
                            .or_else(|| rest.strip_suffix(".idx"))
                    })
                    .filter(|id| is_hex(id, hex_len));
                if id.is_none() || !file_type.is_file() {
                    return Err(refuse(&path, "is not a pack or pack index"));
                }
            }
        } else if is_hex(&name, 2) {
            for (name, path, file_type) in entries(&path)? {
                if !is_hex(&name, hex_len.saturating_sub(2)) || !file_type.is_file() {
                    return Err(refuse(&path, "is not a loose object"));
                }
            }
        } else {
            return Err(refuse(&path, "is not allowed"));
        }
    }
    if !alternates_found {
        return Err(refuse(&objects.join("info/alternates"), "is missing"));
    }
    Ok(())
}

/// Fails unless `path` is a directory, not a symlink.
fn real_dir(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(())
}

/// A branch's name without `refs/heads/`; any other ref whole.
fn short(name: &str) -> &str {
    name.strip_prefix("refs/heads/").unwrap_or(name)
}
