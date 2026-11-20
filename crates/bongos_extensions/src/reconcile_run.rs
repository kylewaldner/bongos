//! The startup reconcile: repairs what a crash left in the extensions directory. It reads `git/`
//! and `installed/` into a [`Snapshot`], asks [`plan`] what to fix and applies each fix to the real
//! folders. It builds, fetches and contacts nothing: the only git call reads a checkout's `origin`
//! from its local configuration. An id whose lock another process holds is never touched, and an
//! extensions directory without `git/` is left exactly as it is.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use bongos_extensions_model::{
    CheckoutSnapshot, ExtensionSnapshot, Fix, InstallRecord, LanguageList, Layout, ReadOutcome,
    RecordSnapshot, RecordState, Snapshot, SourceUrl, SymlinkSnapshot, SymlinkTarget,
    is_reserved_id, plan, read, remove_torn_temp_files, validate_id,
};
use gpui::{AppContext as _, AsyncApp};

use crate::build_log::{Clock, rfc3339};
use crate::git::GitRunner;
use crate::host::HostApi;
use crate::installer::{BUILD_MISSING_KEY, RecordWriter, try_lock};

/// What the reconcile reaches outside itself.
pub(crate) struct Inputs {
    pub layout: Layout,
    pub git: Arc<dyn GitRunner>,
    pub host: Rc<dyn HostApi>,
    pub clock: Arc<dyn Clock>,
    pub list: Arc<LanguageList>,
    pub write_record: RecordWriter,
    /// Ids (and lock keys) this process is changing: treated like locks held elsewhere.
    pub busy: Vec<String>,
}

/// The snapshot and what is needed to act on it.
struct Scan {
    snapshot: Snapshot,
    /// `(id, folder name in the snapshot)` to the real folder name under `git/<id>/`, where they
    /// differ.
    folders: HashMap<(String, String), String>,
    /// The part of each id's own link below its checkout folder.
    link_subdirs: HashMap<String, PathBuf>,
    /// Ids whose lock is held elsewhere.
    held: BTreeSet<String>,
    /// The locks taken, held until the reconcile ends.
    _locks: Vec<File>,
}

impl Scan {
    fn real_folder<'a>(&'a self, id: &str, folder: &'a str) -> &'a str {
        self.folders
            .get(&(id.to_string(), folder.to_string()))
            .map_or(folder, String::as_str)
    }

    fn record(&self, id: &str) -> Option<&InstallRecord> {
        self.snapshot
            .extensions
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| match &e.record {
                RecordSnapshot::Some(record) => Some(record),
                _ => None,
            })
    }
}

/// Repairs the extensions directory and returns the fixes it attempted. A fix that fails is
/// logged and the others still run.
pub(crate) async fn run(inputs: Inputs, cx: &mut AsyncApp) -> Result<Vec<Fix>> {
    let scan = {
        let layout = inputs.layout.clone();
        let busy = inputs.busy.clone();
        cx.background_spawn(async move { scan(&layout, &busy) })
            .await?
    };
    let fixes: Vec<Fix> = plan(&scan.snapshot)
        .into_iter()
        .filter(|fix| fix_id(fix).is_none_or(|id| !scan.held.contains(id)))
        .collect();
    for fix in &fixes {
        match apply(&inputs, &scan, fix, cx).await {
            Ok(()) => log::info!("language extensions: repaired after a crash: {fix:?}"),
            Err(err) => log::error!("language extensions: could not apply {fix:?}: {err:#}"),
        }
    }
    Ok(fixes)
}

fn fix_id(fix: &Fix) -> Option<&str> {
    match fix {
        Fix::DeleteStaging { .. } => None,
        Fix::DeleteCheckout { id, .. }
        | Fix::FinishRemoval { id }
        | Fix::WriteRecordFromSymlink { id, .. }
        | Fix::Relink { id, .. }
        | Fix::MarkBuildMissing { id } => Some(id),
    }
}

/// The commit a checkout folder holds: its name without a `.rebuild-<nonce>` suffix, which a
/// build beside a live checkout of the same commit carries.
pub fn commit_of_folder(folder: &str) -> &str {
    match folder.split_once(".rebuild-") {
        Some((commit, _)) if !commit.is_empty() => commit,
        _ => folder,
    }
}

fn names(dir: &Path, only_dirs: bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| !only_dirs || entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().to_str().map(String::from))
        .collect();
    names.sort();
    names
}

/// A link into `git/<id>/<folder>/<subdir>`.
struct Link {
    link_id: String,
    id: String,
    folder: String,
    subdir: PathBuf,
}

fn read_links(layout: &Layout) -> (Vec<Link>, Vec<String>) {
    let installed = layout.installed_dir();
    let mut links = Vec::new();
    let mut elsewhere = Vec::new();
    for name in names(&installed, false) {
        let path = installed.join(&name);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let Ok(target) = std::fs::read_link(&path) else {
            continue;
        };
        let target = installed.join(target);
        match into_git(layout, &target) {
            Some((id, folder, subdir)) => links.push(Link {
                link_id: name,
                id,
                folder,
                subdir,
            }),
            None => elsewhere.push(name),
        }
    }
    (links, elsewhere)
}

/// Splits a link target inside `git/` into `(id, folder, subdir)`.
fn into_git(layout: &Layout, target: &Path) -> Option<(String, String, PathBuf)> {
    let rest = target.strip_prefix(layout.git_dir()).ok()?;
    if rest
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let mut components = rest.components();
    let id = components.next()?.as_os_str().to_str()?.to_string();
    let folder = components.next()?.as_os_str().to_str()?.to_string();
    Some((id, folder, components.as_path().to_path_buf()))
}

struct Locker<'a> {
    layout: &'a Layout,
    busy: &'a [String],
    tried: HashMap<String, bool>,
    locks: Vec<File>,
}

impl Locker<'_> {
    /// Whether `key` is held elsewhere; otherwise takes its lock until the reconcile ends.
    fn held(&mut self, key: &str) -> bool {
        if let Some(held) = self.tried.get(key) {
            return *held;
        }
        let held = self.busy.iter().any(|b| b == key)
            || match try_lock(&self.layout.lock_path(key)) {
                Ok(Some(file)) => {
                    self.locks.push(file);
                    false
                }
                Ok(None) => true,
                Err(err) => {
                    log::warn!("language extensions: {err:#}");
                    true
                }
            };
        self.tried.insert(key.to_string(), held);
        held
    }
}

fn scan(layout: &Layout, busy: &[String]) -> Result<Scan> {
    let mut scan = Scan {
        snapshot: Snapshot {
            git_dir_exists: false,
            staging: Vec::new(),
            extensions: Vec::new(),
            symlinks: Vec::new(),
            locked_ids_without_folder: Vec::new(),
        },
        folders: HashMap::new(),
        link_subdirs: HashMap::new(),
        held: BTreeSet::new(),
        _locks: Vec::new(),
    };
    let git_dir = layout.git_dir();
    if !git_dir.is_dir() {
        return Ok(scan);
    }
    scan.snapshot.git_dir_exists = true;
    scan.snapshot.staging = names(&layout.staging_dir(), false);

    let mut locker = Locker {
        layout,
        busy,
        tried: HashMap::new(),
        locks: Vec::new(),
    };
    let (links, _) = read_links(layout);
    let ids: Vec<String> = names(&git_dir, true)
        .into_iter()
        .filter(|id| !is_reserved_id(id) && validate_id(id).is_ok())
        .collect();

    for id in &ids {
        if locker.held(id) {
            scan.held.insert(id.clone());
            scan.snapshot.extensions.push(ExtensionSnapshot {
                id: id.clone(),
                checkouts: Vec::new(),
                record: RecordSnapshot::None,
                lock_held: true,
            });
            continue;
        }
        let dir = layout.extension_dir(id);
        if let Err(err) = remove_torn_temp_files(&dir) {
            log::warn!("language extensions: {err:#}");
        }
        let record = match read(&layout.record_path(id)) {
            Ok(ReadOutcome::Ok(record)) => RecordSnapshot::Some(record),
            Ok(ReadOutcome::Missing) => RecordSnapshot::None,
            Ok(ReadOutcome::Unreadable) | Err(_) => RecordSnapshot::Unreadable,
        };
        let own_link = links.iter().find(|l| l.link_id == *id && l.id == *id);
        if let Some(link) = own_link {
            scan.link_subdirs.insert(id.clone(), link.subdir.clone());
        }
        let real_folders = names(&dir, true);
        let subdir = match &record {
            RecordSnapshot::Some(record) => record.path.as_ref().map(PathBuf::from),
            _ => own_link.map(|l| l.subdir.clone()),
        }
        .unwrap_or_default();
        let has_manifest = |folder: &str| {
            dir.join(folder)
                .join(&subdir)
                .join("extension.toml")
                .is_file()
        };

        // Snapshot names: the real names, except that a `<commit>.rebuild-<nonce>` folder holding
        // the live build of its commit is named after the commit.
        let mut logical: Vec<(String, String)> = real_folders
            .iter()
            .map(|f| (f.clone(), f.clone()))
            .collect();
        let mut rename = |from_real: &str, to: String| {
            if let Some(entry) = logical.iter_mut().find(|(_, real)| real == from_real) {
                entry.0 = to;
            }
        };
        match own_link {
            Some(link) if real_folders.contains(&link.folder) => {
                let commit = commit_of_folder(&link.folder);
                if commit != link.folder {
                    if real_folders.iter().any(|f| f == commit) {
                        // Superseded by the linked build beside it.
                        rename(commit, format!("{commit}.superseded"));
                    }
                    rename(&link.folder, commit.to_string());
                }
            }
            Some(_) => {}
            None => {
                if let RecordSnapshot::Some(record) = &record
                    && !real_folders.contains(&record.commit)
                    && let Some(rebuilt) = real_folders.iter().find(|f| {
                        commit_of_folder(f) == record.commit
                            && **f != record.commit
                            && has_manifest(f)
                    })
                {
                    rename(rebuilt, record.commit.clone());
                }
            }
        }
        let mut checkouts = Vec::new();
        for (name, real) in logical {
            checkouts.push(CheckoutSnapshot {
                folder: name.clone(),
                has_manifest: has_manifest(&real),
            });
            if name != real {
                scan.folders.insert((id.clone(), name), real);
            }
        }
        scan.snapshot.extensions.push(ExtensionSnapshot {
            id: id.clone(),
            checkouts,
            record,
            lock_held: false,
        });
    }

    for link in &links {
        let commit = scan
            .folders
            .iter()
            .find(|((id, _), real)| *id == link.id && **real == link.folder)
            .map(|((_, name), _)| name.clone())
            .unwrap_or_else(|| commit_of_folder(&link.folder).to_string());
        if validate_id(&link.link_id).is_ok()
            && !ids.contains(&link.link_id)
            && locker.held(&link.link_id)
        {
            scan.held.insert(link.link_id.clone());
        }
        scan.snapshot.symlinks.push(SymlinkSnapshot {
            id: link.link_id.clone(),
            target: SymlinkTarget::Checkout {
                id: link.id.clone(),
                commit,
                origin_url: None,
            },
        });
    }

    // Any other lock held elsewhere (an install whose id is not known yet holds its source's
    // lock) may be using staging.
    let lock_keys: Vec<String> = names(&layout.locks_dir(), false)
        .into_iter()
        .filter_map(|name| name.strip_suffix(".lock").map(String::from))
        .chain(busy.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for key in lock_keys {
        if !ids.contains(&key) && locker.held(&key) {
            scan.held.insert(key.clone());
            scan.snapshot.locked_ids_without_folder.push(key);
        }
    }
    scan._locks = locker.locks;
    Ok(scan)
}

async fn apply(inputs: &Inputs, scan: &Scan, fix: &Fix, cx: &mut AsyncApp) -> Result<()> {
    let layout = inputs.layout.clone();
    match fix {
        Fix::DeleteStaging { nonce } => {
            let path = layout.staging_for(nonce);
            cx.background_spawn(async move { remove_path(&path) }).await
        }
        Fix::DeleteCheckout { id, folder } => {
            let path = layout.checkout_dir(id, scan.real_folder(id, folder));
            cx.background_spawn(async move {
                // A link from anywhere into the folder keeps it.
                let (links, _) = read_links(&layout);
                if links
                    .iter()
                    .any(|l| layout.checkout_dir(&l.id, &l.folder) == path)
                {
                    return Err(anyhow!("{} is linked; kept", path.display()));
                }
                remove_path(&path)
            })
            .await
        }
        Fix::FinishRemoval { id } => {
            let unlinked = {
                let layout = layout.clone();
                let id = id.clone();
                cx.background_spawn(async move { remove_own_link(&layout, &id) })
                    .await?
            };
            if unlinked {
                let unlink = cx.update(|cx| inputs.host.unlink(id, cx));
                if let Err(err) = unlink.await {
                    log::warn!("language extensions: could not unload {id}: {err:#}");
                }
            }
            let dir = layout.extension_dir(id);
            cx.background_spawn(async move { remove_path(&dir) }).await
        }
        Fix::WriteRecordFromSymlink {
            id,
            commit,
            origin_url,
        } => {
            let checkout = layout.checkout_dir(id, scan.real_folder(id, commit));
            let origin = match origin_url {
                Some(origin) => origin.clone(),
                None => {
                    let git = inputs.git.clone();
                    let dir = checkout.clone();
                    cx.background_spawn(async move { git.remote_url(&dir).await })
                        .await
                        .with_context(|| {
                            format!("could not read the origin of {}", checkout.display())
                        })?
                }
            };
            let url = SourceUrl::parse(&origin)?;
            let path = scan
                .link_subdirs
                .get(id)
                .and_then(|subdir| subdir.to_str())
                .filter(|subdir| !subdir.is_empty())
                .map(String::from);
            let listed = inputs.list.is_listed(&url, path.as_deref(), commit);
            let previous = scan.record(id);
            let mut extra = previous.map(|r| r.extra.clone()).unwrap_or_default();
            extra.remove(BUILD_MISSING_KEY);
            let record = InstallRecord {
                version: 1,
                id: id.clone(),
                source_url: url.as_str().to_string(),
                path,
                requested_ref: None,
                commit: commit.clone(),
                listed,
                // It was installed, so it was trusted then.
                trusted_commit: (!listed).then(|| commit.clone()),
                state: RecordState::Installed,
                installed_at: rfc3339(inputs.clock.now()),
                extra,
            };
            write(inputs, &layout, record, cx).await
        }
        Fix::Relink { id, commit } => {
            let record = scan
                .record(id)
                .ok_or_else(|| anyhow!("{id} has no record"))?;
            let root = layout
                .checkout_dir(id, scan.real_folder(id, commit))
                .join(record.path.as_deref().unwrap_or_default());
            let link = cx.update(|cx| inputs.host.link(id, root, cx));
            link.await
        }
        Fix::MarkBuildMissing { id } => match scan.record(id) {
            Some(record) => {
                let mut record = record.clone();
                record
                    .extra
                    .insert(BUILD_MISSING_KEY.to_string(), serde_json::Value::Bool(true));
                write(inputs, &layout, record, cx).await
            }
            None => {
                log::warn!(
                    "language extensions: the build of {id} is missing and it has no record"
                );
                Ok(())
            }
        },
    }
}

async fn write(
    inputs: &Inputs,
    layout: &Layout,
    record: InstallRecord,
    cx: &mut AsyncApp,
) -> Result<()> {
    let write_record = inputs.write_record;
    let path = layout.record_path(&record.id);
    cx.background_spawn(async move {
        let nonce = format!("{}-reconcile", std::process::id());
        write_record(&path, &record, &nonce)
            .with_context(|| format!("could not write {}", path.display()))
    })
    .await
}

/// Removes `installed/<id>` when it points into `git/<id>/`; returns whether it did.
fn remove_own_link(layout: &Layout, id: &str) -> Result<bool> {
    let link = layout.symlink_path(id);
    let ours = std::fs::read_link(&link).is_ok_and(|t| t.starts_with(layout.extension_dir(id)));
    if ours {
        std::fs::remove_file(&link)
            .with_context(|| format!("could not remove {}", link.display()))?;
    }
    Ok(ours)
}

fn remove_path(path: &Path) -> Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    };
    result.with_context(|| format!("could not remove {}", path.display()))
}

#[cfg(test)]
#[path = "reconcile_run_tests.rs"]
mod tests;
