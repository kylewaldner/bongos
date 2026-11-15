//! `java: show jdk`: the JDKs Java uses and which server serves Java in each worktree.

use std::path::{Path, PathBuf};

use bongos_kotlin::{JavaRoutes, JavaServer, JdkInputs, java_sections};
use gpui::{App, actions};
use settings::{Settings as _, SettingsLocation, WorktreeId};
use util::rel_path::RelPath;

use crate::{JavaSettings, JdkEnvironment, MIN_SERVER_JDK};

actions!(
    java,
    [
        /// Shows the JDK that runs the Java server, the JDK that imports projects, why each was
        /// chosen, and which server serves Java in each worktree.
        ShowJdk,
    ]
);

/// The title of the buffer `ShowJdk` opens.
pub const SHOW_JDK_TITLE: &str = "Java JDK";

/// The name of the import JDK setting.
const JAVA_JDK_HOME: &str = "java.jdk_home";

/// Everything `java: show jdk` reads for one worktree on the main thread.
#[derive(Clone, Debug)]
pub struct JdkQuery {
    /// The import JDK inputs, with the `java.jdk_home` setting.
    pub inputs: JdkInputs,
    /// The `java.server_jdk` setting.
    pub server_setting: Option<PathBuf>,
    /// The server that serves Java in the worktree.
    pub server: JavaServer,
}

impl JdkQuery {
    /// The text of one worktree's section: the root, the serving server, then both JDKs.
    pub fn text(&self) -> String {
        let serving = match self.server {
            JavaServer::Jdtls => "jdtls".to_string(),
            JavaServer::Kotlin => "kotlin (the IntelliJ server; the JDKs below are for jdtls, \
                see kotlin: show jdk for the IntelliJ server's)"
                .to_string(),
        };
        format!(
            "{}\nJava server: {serving}\n{}",
            self.inputs.root.display(),
            java_sections(&self.inputs, self.server_setting.as_deref(), MIN_SERVER_JDK)
        )
    }
}

/// Everything `java: show jdk` reads on the main thread. `text` does the file reads and can run on
/// any thread.
pub struct ShowJdkRequest {
    queries: Vec<JdkQuery>,
}

impl ShowJdkRequest {
    /// `worktrees` are the local visible worktrees: root and id. Reads settings and the cached
    /// Java server of each, never the disk.
    pub fn new(
        worktrees: impl IntoIterator<Item = (PathBuf, WorktreeId)>,
        env: &JdkEnvironment,
        cx: &App,
    ) -> Self {
        let queries = worktrees
            .into_iter()
            .map(|(root, worktree_id)| query(&root, worktree_id, env, cx))
            .collect();
        Self { queries }
    }

    /// The contents of the "Java JDK" buffer.
    pub fn text(&self) -> String {
        report(&self.queries)
    }
}

fn query(root: &Path, worktree_id: WorktreeId, env: &JdkEnvironment, cx: &App) -> JdkQuery {
    let java = JavaSettings::get(
        Some(SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        }),
        cx,
    );
    JdkQuery {
        inputs: JdkInputs {
            setting: java.jdk_home.clone(),
            setting_name: JAVA_JDK_HOME,
            project_settings: Some(root.join(".bongos").join("settings.json")),
            root: root.to_path_buf(),
            java_home: env.java_home.clone(),
            candidates: env.candidates.clone(),
        },
        server_setting: java.server_jdk.clone(),
        server: JavaRoutes::server(worktree_id, cx),
    }
}

/// The whole "Java JDK" buffer for the given worktrees.
pub fn report(queries: &[JdkQuery]) -> String {
    if queries.is_empty() {
        return "No local project is open.\n".into();
    }
    queries
        .iter()
        .map(JdkQuery::text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gpui::TestAppContext;
    use settings::SettingsStore;

    use super::*;

    fn write_jdk(base: &Path, name: &str, version: &str) -> PathBuf {
        let home = base.join(name);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("release"),
            format!("JAVA_VERSION=\"{version}\"\n"),
        )
        .unwrap();
        home
    }

    /// A project root and a JDK directory under a temp dir; discovery sees only that directory.
    fn env_in(base: &Path) -> JdkEnvironment {
        JdkEnvironment {
            java_home: None,
            candidates: vec![format!("{}/jdks/*", base.display())],
        }
    }

    fn query_in(base: &Path, server: JavaServer) -> JdkQuery {
        let root = base.join("project");
        fs::create_dir_all(&root).unwrap();
        let env = env_in(base);
        JdkQuery {
            inputs: JdkInputs {
                setting: None,
                setting_name: JAVA_JDK_HOME,
                project_settings: Some(root.join(".bongos/settings.json")),
                root,
                java_home: env.java_home,
                candidates: env.candidates,
            },
            server_setting: None,
            server,
        }
    }

    #[test]
    fn test_text_for_a_jdtls_worktree_shows_both_jdks() {
        let temp = tempfile::tempdir().unwrap();
        write_jdk(&temp.path().join("jdks"), "jdk-17", "17.0.9");
        write_jdk(&temp.path().join("jdks"), "jdk-25", "25.0.1");
        let query = query_in(temp.path(), JavaServer::Jdtls);

        let text = report(&[query]);
        let root = temp.path().join("project");
        assert!(
            text.starts_with(&format!("{}\nJava server: jdtls\n", root.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "Server JDK: {}/jdks/jdk-25\n",
                temp.path().display()
            )),
            "{text}"
        );
        assert!(text.contains("Version: 25.0.1\n"), "{text}");
        assert!(
            text.contains("Reason: newest JDK 25 or newer, found in "),
            "{text}"
        );
        assert!(text.contains("Import JDK: "), "{text}");
    }

    #[test]
    fn test_text_for_an_intellij_worktree_names_the_kotlin_server() {
        let temp = tempfile::tempdir().unwrap();
        write_jdk(&temp.path().join("jdks"), "jdk-25", "25.0.1");
        let query = query_in(temp.path(), JavaServer::Kotlin);

        let text = report(&[query]);
        assert!(
            text.contains("Java server: kotlin (the IntelliJ server;"),
            "{text}"
        );
        assert!(!text.contains("Java server: jdtls"), "{text}");
        assert!(text.contains("Server JDK: "), "{text}");
        assert!(text.contains("Import JDK: "), "{text}");
    }

    #[test]
    fn test_reasons_name_java_settings_and_missing_jdks_say_why() {
        let temp = tempfile::tempdir().unwrap();
        let chosen = write_jdk(temp.path(), "chosen", "21.0.2");
        let mut query = query_in(temp.path(), JavaServer::Jdtls);
        query.inputs.setting = Some(chosen);

        let text = report(&[query]);
        assert!(
            text.contains("Reason: java.jdk_home in settings\n"),
            "{text}"
        );
        assert!(
            text.contains("Server JDK: none\nReason: jdtls needs JDK 25 or newer (found none)\n"),
            "{text}"
        );
    }

    #[test]
    fn test_no_worktree_is_reported() {
        assert_eq!(report(&[]), "No local project is open.\n");
    }

    #[gpui::test]
    fn test_request_reads_settings_and_the_cached_server(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            JavaSettings::register(cx);
        });
        let temp = tempfile::tempdir().unwrap();
        write_jdk(&temp.path().join("jdks"), "jdk-26", "26.0.0");
        let env = env_in(temp.path());
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();

        let request = cx.update(|cx| {
            ShowJdkRequest::new([(root.clone(), WorktreeId::from_usize(1))], &env, cx)
        });
        let text = request.text();
        // No manager knows the worktree, so it runs jdtls.
        assert!(text.contains("Java server: jdtls\n"), "{text}");
        assert!(text.contains("Version: 26.0.0\n"), "{text}");
    }
}
