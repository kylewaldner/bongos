//! Startup reconciliation of the extensions directory: a pure plan from a directory snapshot.

use std::collections::{BTreeMap, BTreeSet};

use crate::InstallRecord;
use crate::RecordState;
use crate::validate_id;

#[derive(Debug, Clone, PartialEq)]
pub enum RecordSnapshot {
    None,
    Unreadable,
    Some(InstallRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSnapshot {
    /// Folder name under `git/<id>/`.
    pub folder: String,
    pub has_manifest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionSnapshot {
    pub id: String,
    pub checkouts: Vec<CheckoutSnapshot>,
    pub record: RecordSnapshot,
    pub lock_held: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymlinkTarget {
    /// Points into `git/<id>/<commit>`, possibly through a subdirectory.
    Checkout {
        id: String,
        commit: String,
        origin_url: Option<String>,
    },
    /// Points anywhere else: not ours, never touched.
    Elsewhere,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkSnapshot {
    pub id: String,
    pub target: SymlinkTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub git_dir_exists: bool,
    pub staging: Vec<String>,
    pub extensions: Vec<ExtensionSnapshot>,
    pub symlinks: Vec<SymlinkSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fix {
    DeleteStaging {
        nonce: String,
    },
    DeleteCheckout {
        id: String,
        folder: String,
    },
    /// Remove the symlink and delete the id's folder entirely.
    FinishRemoval {
        id: String,
    },
    WriteRecordFromSymlink {
        id: String,
        commit: String,
        origin_url: Option<String>,
    },
    Relink {
        id: String,
        commit: String,
    },
    MarkBuildMissing {
        id: String,
    },
}

impl Fix {
    fn id(&self) -> &str {
        match self {
            Fix::DeleteStaging { .. } => "",
            Fix::DeleteCheckout { id, .. }
            | Fix::FinishRemoval { id }
            | Fix::WriteRecordFromSymlink { id, .. }
            | Fix::Relink { id, .. }
            | Fix::MarkBuildMissing { id } => id,
        }
    }
}

/// Decides what to repair. Does no filesystem access; output is sorted by id, then fix kind.
///
/// Extension and symlink ids that fail `validate_id` (including reserved names in any ASCII case)
/// are skipped. Staging folders are not scoped to an id, so no `DeleteStaging` is emitted while
/// any extension holds its lock: an install in another process may be using staging.
pub fn plan(snapshot: &Snapshot) -> Vec<Fix> {
    if !snapshot.git_dir_exists {
        return Vec::new();
    }
    let mut fixes: Vec<Fix> = Vec::new();
    if !snapshot.extensions.iter().any(|e| e.lock_held) {
        fixes.extend(snapshot.staging.iter().map(|nonce| Fix::DeleteStaging {
            nonce: nonce.clone(),
        }));
    }

    // Symlinks that point into git/<id>/ belong to that id, if it is the link's own id. A link
    // aimed at another id's checkout gets no fix, but protects that checkout from deletion.
    let mut links: BTreeMap<&str, (&str, Option<&str>)> = BTreeMap::new();
    let mut protected: BTreeSet<(&str, &str)> = BTreeSet::new();
    for link in &snapshot.symlinks {
        if let SymlinkTarget::Checkout {
            id,
            commit,
            origin_url,
        } = &link.target
        {
            if validate_id(&link.id).is_err() || validate_id(id).is_err() {
                continue;
            }
            if *id == link.id {
                links.insert(id, (commit, origin_url.as_deref()));
            } else {
                protected.insert((id, commit));
            }
        }
    }
    let known: BTreeSet<&str> = snapshot.extensions.iter().map(|e| e.id.as_str()).collect();
    let orphan_links: Vec<ExtensionSnapshot> = links
        .keys()
        .filter(|id| !known.contains(*id))
        .map(|id| ExtensionSnapshot {
            id: id.to_string(),
            checkouts: Vec::new(),
            record: RecordSnapshot::None,
            lock_held: false,
        })
        .collect();

    for extension in snapshot.extensions.iter().chain(&orphan_links) {
        if extension.lock_held || validate_id(&extension.id).is_err() {
            continue;
        }
        plan_extension(
            extension,
            links.get(extension.id.as_str()).copied(),
            &protected,
            &mut fixes,
        );
    }
    fixes.sort_by(|a, b| a.id().cmp(b.id()).then_with(|| a.cmp(b)));
    fixes
}

/// Adopts the symlink's commit into the record, or marks the build missing when the checkout it
/// points at does not exist.
fn adopt_link(
    extension: &ExtensionSnapshot,
    commit: &str,
    origin_url: Option<&str>,
    fixes: &mut Vec<Fix>,
) {
    let id = extension.id.clone();
    if extension.checkouts.iter().any(|c| c.folder == commit) {
        fixes.push(Fix::WriteRecordFromSymlink {
            id,
            commit: commit.to_string(),
            origin_url: origin_url.map(str::to_string),
        });
    } else {
        fixes.push(Fix::MarkBuildMissing { id });
    }
}

fn plan_extension(
    extension: &ExtensionSnapshot,
    link: Option<(&str, Option<&str>)>,
    protected: &BTreeSet<(&str, &str)>,
    fixes: &mut Vec<Fix>,
) {
    let id = &extension.id;
    let mut keep: BTreeSet<&str> = BTreeSet::new();
    let mut delete_others = true;
    match &extension.record {
        RecordSnapshot::Some(record) if record.id != *id => {
            // A record that belongs to another id cannot be trusted: touch nothing.
            return;
        }
        RecordSnapshot::Some(record) if record.state == RecordState::Removing => {
            if !protected.iter().any(|(protected_id, _)| protected_id == id) {
                fixes.push(Fix::FinishRemoval { id: id.clone() });
            }
            return;
        }
        RecordSnapshot::Some(record) => match link {
            Some((commit, origin_url)) => {
                keep.insert(commit);
                if commit != record.commit {
                    adopt_link(extension, commit, origin_url, fixes);
                }
            }
            None => {
                keep.insert(&record.commit);
                let has_manifest = extension
                    .checkouts
                    .iter()
                    .any(|c| c.folder == record.commit && c.has_manifest);
                fixes.push(if has_manifest {
                    Fix::Relink {
                        id: id.clone(),
                        commit: record.commit.clone(),
                    }
                } else {
                    Fix::MarkBuildMissing { id: id.clone() }
                });
            }
        },
        RecordSnapshot::None | RecordSnapshot::Unreadable => match link {
            Some((commit, origin_url)) => {
                keep.insert(commit);
                adopt_link(extension, commit, origin_url, fixes);
            }
            // The source is unknown: leave the record alone and delete nothing without a link.
            None => delete_others = extension.record == RecordSnapshot::None,
        },
    }
    if delete_others {
        for checkout in &extension.checkouts {
            if !keep.contains(checkout.folder.as_str())
                && !protected.contains(&(id.as_str(), checkout.folder.as_str()))
            {
                fixes.push(Fix::DeleteCheckout {
                    id: id.clone(),
                    folder: checkout.folder.clone(),
                });
            }
        }
    }
}
