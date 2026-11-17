//! Java on the Kotlin server. The `kotlin` adapter is also registered for Java, and a policy picks
//! per worktree which of `kotlin` and `jdtls` serves Java: `kotlin` where the effective Kotlin
//! server is IntelliJ and IntelliJ is installed or may be downloaded, `jdtls` everywhere else. One
//! adapter name serves both languages, so a mixed project on IntelliJ runs one server process.
//!
//! The answer of each worktree is cached here by its `KotlinProjectManager`; the policy only reads
//! the cache. A change of the answer is a switch: while it runs, Java allows neither server and
//! Kotlin does not allow `kotlin` in that worktree, so `reapply_server_policy` stops every old
//! server before any new one starts. Java never has both servers allowed, not even mid-switch.

use std::collections::HashMap;

use gpui::{App, Global};
use language::LanguageName;
use lsp::LanguageServerName;
use project::{LanguageServerPolicy, PolicyWorktree};
use settings::WorktreeId;

use crate::manager::ADAPTER_NAME;

/// The language Kotlin's server also serves on IntelliJ.
pub const JAVA_LANGUAGE: &str = "Java";

/// The language server identifier of Java documents sent to the `kotlin` server.
pub const JAVA_LANGUAGE_ID: &str = "java";

/// The name of the Java adapter that serves Java wherever the IntelliJ server does not.
pub const JDTLS_ADAPTER: &str = "jdtls";

/// The key of the mixed JVM notice.
pub const MIXED_JVM_NOTICE_KEY: &str = "mixed-jvm";

/// The mixed JVM notice: Kotlin on kotlin-lsp and Java on jdtls in the same worktree.
pub const MIXED_JVM_NOTICE: &str = "Kotlin and Java use separate servers here and do not see each other's symbols; the IntelliJ server serves both (see kotlin.server)";

const KOTLIN_LANGUAGE: &str = "Kotlin";

/// The server that serves Java in a worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JavaServer {
    /// The `kotlin` adapter, running the IntelliJ server.
    Kotlin,
    /// The `jdtls` adapter.
    #[default]
    Jdtls,
}

#[derive(Clone, Copy, Debug, Default)]
struct Route {
    server: JavaServer,
    /// Switches begun and not yet ended. While any runs, neither Java server is allowed.
    switching: usize,
}

/// Each worktree's cached answer. A worktree no manager knows uses `jdtls`.
#[derive(Default)]
pub struct JavaRoutes {
    routes: HashMap<WorktreeId, Route>,
}

impl Global for JavaRoutes {}

impl JavaRoutes {
    /// The server that serves Java in the worktree once no switch runs.
    pub fn server(worktree_id: WorktreeId, cx: &App) -> JavaServer {
        Self::route(worktree_id, cx).server
    }

    /// Whether a switch of the worktree's Java server is still stopping the old servers.
    pub fn switching(worktree_id: WorktreeId, cx: &App) -> bool {
        Self::route(worktree_id, cx).switching > 0
    }

    fn route(worktree_id: WorktreeId, cx: &App) -> Route {
        cx.try_global::<Self>()
            .and_then(|routes| routes.routes.get(&worktree_id).copied())
            .unwrap_or_default()
    }

    /// Records the worktree's new answer and begins its switch. Call before the
    /// `reapply_server_policy` that applies it, in the same main-thread turn.
    pub(crate) fn begin_switch(worktree_id: WorktreeId, server: JavaServer, cx: &mut App) {
        let route = cx
            .default_global::<Self>()
            .routes
            .entry(worktree_id)
            .or_default();
        route.server = server;
        route.switching += 1;
    }

    /// Ends one switch of the worktree. Returns whether none is left, when the new servers may
    /// start.
    pub(crate) fn end_switch(worktree_id: WorktreeId, cx: &mut App) -> bool {
        let Some(route) = cx.default_global::<Self>().routes.get_mut(&worktree_id) else {
            return true;
        };
        route.switching = route.switching.saturating_sub(1);
        route.switching == 0
    }

    pub(crate) fn remove(worktree_id: WorktreeId, cx: &mut App) {
        cx.default_global::<Self>().routes.remove(&worktree_id);
    }
}

/// Allows `kotlin` or `jdtls` for Java by each worktree's cached answer (decision D12).
pub struct JavaServerPolicy;

impl LanguageServerPolicy for JavaServerPolicy {
    fn allows(
        &self,
        worktree: &PolicyWorktree,
        language: &LanguageName,
        server: &LanguageServerName,
        cx: &App,
    ) -> bool {
        allows(
            JavaRoutes::route(worktree.id, cx),
            language.as_ref(),
            server.0.as_ref(),
        )
    }
}

fn allows(route: Route, language: &str, server: &str) -> bool {
    let switching = route.switching > 0;
    match (language, server) {
        (JAVA_LANGUAGE, ADAPTER_NAME) => !switching && route.server == JavaServer::Kotlin,
        (JAVA_LANGUAGE, JDTLS_ADAPTER) => !switching && route.server == JavaServer::Jdtls,
        // The old Kotlin server must be gone before the new one starts for Kotlin and Java.
        (KOTLIN_LANGUAGE, ADAPTER_NAME) => !switching,
        _ => true,
    }
}

#[cfg(test)]
#[path = "java_tests.rs"]
mod tests;

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn test_default_language_ids_match_the_kotlin_adapter_ids() {
        // The Kotlin adapter does not override `language_ids`: the default id of a language is its
        // lowercase name, which is what the server expects for both languages.
        assert_eq!(
            LanguageName::new_static(JAVA_LANGUAGE).lsp_id(),
            JAVA_LANGUAGE_ID
        );
        assert_eq!(LanguageName::new_static(KOTLIN_LANGUAGE).lsp_id(), "kotlin");
    }

    #[test]
    fn test_java_never_allows_both_servers() {
        for server in [JavaServer::Kotlin, JavaServer::Jdtls] {
            for switching in [0, 1, 2] {
                let route = Route { server, switching };
                let kotlin = allows(route, JAVA_LANGUAGE, ADAPTER_NAME);
                let jdtls = allows(route, JAVA_LANGUAGE, JDTLS_ADAPTER);
                assert!(!(kotlin && jdtls), "{route:?}");
                assert_eq!(kotlin, switching == 0 && server == JavaServer::Kotlin);
                assert_eq!(jdtls, switching == 0 && server == JavaServer::Jdtls);
                assert_eq!(allows(route, KOTLIN_LANGUAGE, ADAPTER_NAME), switching == 0);
                assert!(allows(route, "Rust", "rust-analyzer"));
            }
        }
        // A worktree no manager knows runs jdtls.
        let unknown = Route::default();
        assert!(allows(unknown, JAVA_LANGUAGE, JDTLS_ADAPTER));
        assert!(!allows(unknown, JAVA_LANGUAGE, ADAPTER_NAME));
    }
}
