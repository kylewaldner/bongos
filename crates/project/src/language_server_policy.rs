//! A per-worktree policy that decides which language servers may run.
//!
//! The `language_servers` setting picks the candidate servers for a language; every registered
//! [`LanguageServerPolicy`] can then refuse a candidate for a given worktree. A server is started
//! only when every registered policy allows it. With no policy registered nothing changes.

use std::{path::Path, sync::Arc};

use gpui::{App, Global};
use language::{Buffer, LanguageName};
use lsp::LanguageServerName;
use settings::WorktreeId;
use util::rel_path::RelPath;
use worktree::{File, Worktree};

/// Decides, per worktree, which language servers may run and which server formats a file.
pub trait LanguageServerPolicy: 'static + Send + Sync {
    /// Called on the main thread for every (worktree, language, server) the server tree considers.
    /// Must answer from cached state, never from disk or the network.
    fn allows(
        &self,
        worktree: &PolicyWorktree,
        language: &LanguageName,
        server: &LanguageServerName,
        cx: &App,
    ) -> bool;

    /// Replaces Prettier as the "auto" formatter for a file, naming the language server to use.
    fn formatter_server(
        &self,
        _worktree: &PolicyWorktree,
        _language: &LanguageName,
        _cx: &App,
    ) -> Option<LanguageServerName> {
        None
    }
}

/// The worktree a policy question is about.
pub struct PolicyWorktree<'a> {
    pub id: WorktreeId,
    pub abs_path: &'a Path,
    /// Whether the worktree snapshot has an entry at the given path. Never touches the disk.
    pub has_entry: &'a dyn Fn(&RelPath) -> bool,
}

#[derive(Default)]
struct LanguageServerPolicies(Vec<Arc<dyn LanguageServerPolicy>>);

impl Global for LanguageServerPolicies {}

/// Registers a policy. Call [`crate::LspStore::reapply_server_policy`] afterwards to apply it to
/// servers that are already running.
pub fn register_language_server_policy(policy: Arc<dyn LanguageServerPolicy>, cx: &mut App) {
    cx.default_global::<LanguageServerPolicies>().0.push(policy);
}

/// Runs `f` with a [`PolicyWorktree`] backed by the worktree's current snapshot.
pub(crate) fn with_policy_worktree<R>(
    worktree: &Worktree,
    f: impl FnOnce(&PolicyWorktree) -> R,
) -> R {
    let abs_path = worktree.abs_path();
    let has_entry = |path: &RelPath| worktree.entry_for_path(path).is_some();
    f(&PolicyWorktree {
        id: worktree.id(),
        abs_path: &abs_path,
        has_entry: &has_entry,
    })
}

/// Whether every registered policy allows `server` for `language` in `worktree`.
pub(crate) fn policy_allows(
    worktree: &PolicyWorktree,
    language: &LanguageName,
    server: &LanguageServerName,
    cx: &App,
) -> bool {
    cx.try_global::<LanguageServerPolicies>()
        .is_none_or(|policies| {
            policies
                .0
                .iter()
                .all(|policy| policy.allows(worktree, language, server, cx))
        })
}

/// The first registered policy's choice of formatting server for `language` in `worktree`.
pub(crate) fn policy_formatter_server(
    worktree: &PolicyWorktree,
    language: &LanguageName,
    cx: &App,
) -> Option<LanguageServerName> {
    cx.try_global::<LanguageServerPolicies>()?
        .0
        .iter()
        .find_map(|policy| policy.formatter_server(worktree, language, cx))
}

/// The policy's choice of formatting server for a local buffer, if any.
pub(crate) fn policy_formatter_for_buffer(buffer: &Buffer, cx: &App) -> Option<LanguageServerName> {
    let file = File::from_dyn(buffer.file())?;
    let language = buffer.language()?;
    with_policy_worktree(file.worktree.read(cx), |worktree| {
        policy_formatter_server(worktree, &language.name(), cx)
    })
}
