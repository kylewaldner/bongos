//! Startup reconciliation of the extensions directory: a pure plan from a directory snapshot.

use std::collections::{BTreeMap, BTreeSet};

use crate::InstallRecord;
use crate::RecordState;

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
pub fn plan(snapshot: &Snapshot) -> Vec<Fix> {
    if !snapshot.git_dir_exists {
        return Vec::new();
    }
    let mut fixes: Vec<Fix> = snapshot
        .staging
        .iter()
        .map(|nonce| Fix::DeleteStaging {
            nonce: nonce.clone(),
        })
        .collect();

    // Symlinks that point into git/<id>/ belong to that id, if it is the link's own id.
    let mut links: BTreeMap<&str, (&str, Option<&str>)> = BTreeMap::new();
    for link in &snapshot.symlinks {
        if let SymlinkTarget::Checkout {
            id,
            commit,
            origin_url,
        } = &link.target
            && *id == link.id
        {
            links.insert(id, (commit, origin_url.as_deref()));
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
        if extension.lock_held {
            continue;
        }
        plan_extension(
            extension,
            links.get(extension.id.as_str()).copied(),
            &mut fixes,
        );
    }
    fixes.sort_by(|a, b| a.id().cmp(b.id()).then_with(|| a.cmp(b)));
    fixes
}

fn plan_extension(
    extension: &ExtensionSnapshot,
    link: Option<(&str, Option<&str>)>,
    fixes: &mut Vec<Fix>,
) {
    let id = &extension.id;
    let mut keep: BTreeSet<&str> = BTreeSet::new();
    let mut delete_others = true;
    match &extension.record {
        RecordSnapshot::Some(record) if record.state == RecordState::Removing => {
            fixes.push(Fix::FinishRemoval { id: id.clone() });
            return;
        }
        RecordSnapshot::Some(record) => match link {
            Some((commit, origin_url)) => {
                keep.insert(commit);
                if commit != record.commit {
                    fixes.push(Fix::WriteRecordFromSymlink {
                        id: id.clone(),
                        commit: commit.to_string(),
                        origin_url: origin_url.map(str::to_string),
                    });
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
        RecordSnapshot::None => {
            if let Some((commit, origin_url)) = link {
                keep.insert(commit);
                fixes.push(Fix::WriteRecordFromSymlink {
                    id: id.clone(),
                    commit: commit.to_string(),
                    origin_url: origin_url.map(str::to_string),
                });
            }
        }
        RecordSnapshot::Unreadable => {
            // The source is unknown: leave the record alone and delete nothing without a link.
            match link {
                Some((commit, _)) => {
                    keep.insert(commit);
                }
                None => delete_others = false,
            }
        }
    }
    if delete_others {
        for checkout in &extension.checkouts {
            if !keep.contains(checkout.folder.as_str()) {
                fixes.push(Fix::DeleteCheckout {
                    id: id.clone(),
                    folder: checkout.folder.clone(),
                });
            }
        }
    }
}
