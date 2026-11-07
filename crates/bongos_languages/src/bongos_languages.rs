//! Languages that bongos ships built in, beyond the ones inherited with the editor.
//!
//! The Kotlin config and queries in `src/kotlin/` come from the Zed Kotlin extension
//! (github.com/zed-extensions/kotlin, MIT, see `LICENSE-KOTLIN-EXTENSION`). The grammar is
//! fwcd/tree-sitter-kotlin (MIT) at the commit that extension pins.
//!
//! The Java and TOML configs and queries in `src/java/` and `src/toml/` come from the Zed Java and TOML
//! extensions (github.com/zed-extensions/java and github.com/zed-extensions/toml, Apache-2.0, see
//! `LICENSE-JAVA-EXTENSION` and `LICENSE-TOML-EXTENSION`). The grammars are tree-sitter-java (MIT) and
//! tree-sitter-toml-ng (MIT) from crates.io.
//!
//! Everything here is compiled into the binary: the grammars are linked in and the queries are embedded,
//! so nothing is fetched at runtime.

use std::sync::Arc;

use language::{LanguageConfig, LanguageQueries, LanguageRegistry, LoadedLanguage};

const KOTLIN_CONFIG: &str = include_str!("kotlin/config.toml");
const JAVA_CONFIG: &str = include_str!("java/config.toml");
const TOML_CONFIG: &str = include_str!("toml/config.toml");

/// Registers the built-in languages and their grammars with the registry.
///
/// Registering a language under a name that is already registered replaces it, so a language
/// provided later by an extension never produces a second entry with the same name.
pub fn init(languages: &LanguageRegistry) {
    languages.register_native_grammars([
        ("kotlin", tree_sitter_kotlin::LANGUAGE),
        ("java", tree_sitter_java::LANGUAGE),
        ("toml", tree_sitter_toml_ng::LANGUAGE),
    ]);

    register(languages, KOTLIN_CONFIG, kotlin_queries);
    register(languages, JAVA_CONFIG, java_queries);
    register(languages, TOML_CONFIG, toml_queries);
}

fn register(
    languages: &LanguageRegistry,
    config_source: &'static str,
    queries: fn() -> LanguageQueries,
) {
    let config = parse_config(config_source);
    languages.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(move || {
            Ok(LoadedLanguage {
                config: parse_config(config_source),
                queries: queries(),
                context_provider: None,
                toolchain_provider: None,
                manifest_name: None,
            })
        }),
    );
}

fn parse_config(source: &str) -> LanguageConfig {
    toml::from_str(source).expect("an embedded config.toml is valid")
}

fn kotlin_queries() -> LanguageQueries {
    LanguageQueries {
        highlights: Some(include_str!("kotlin/highlights.scm").into()),
        brackets: Some(include_str!("kotlin/brackets.scm").into()),
        indents: Some(include_str!("kotlin/indents.scm").into()),
        outline: Some(include_str!("kotlin/outline.scm").into()),
        injections: Some(include_str!("kotlin/injections.scm").into()),
        overrides: Some(include_str!("kotlin/overrides.scm").into()),
        ..Default::default()
    }
}

fn java_queries() -> LanguageQueries {
    LanguageQueries {
        highlights: Some(include_str!("java/highlights.scm").into()),
        brackets: Some(include_str!("java/brackets.scm").into()),
        indents: Some(include_str!("java/indents.scm").into()),
        outline: Some(include_str!("java/outline.scm").into()),
        injections: Some(include_str!("java/injections.scm").into()),
        overrides: Some(include_str!("java/overrides.scm").into()),
        ..Default::default()
    }
}

fn toml_queries() -> LanguageQueries {
    LanguageQueries {
        highlights: Some(include_str!("toml/highlights.scm").into()),
        brackets: Some(include_str!("toml/brackets.scm").into()),
        indents: Some(include_str!("toml/indents.scm").into()),
        outline: Some(include_str!("toml/outline.scm").into()),
        injections: Some(include_str!("toml/injections.scm").into()),
        overrides: Some(include_str!("toml/overrides.scm").into()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use gpui::TestAppContext;
    use language::Language;
    use streaming_iterator::StreamingIterator as _;

    use super::*;

    const SNIPPET: &str = "fun main() { val x = 1 }";

    fn registry(cx: &TestAppContext) -> Arc<LanguageRegistry> {
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        init(&registry);
        registry
    }

    #[gpui::test]
    async fn kt_and_kts_select_kotlin(cx: &mut TestAppContext) {
        let registry = registry(cx);
        for path in ["src/Main.kt", "build.gradle.kts", "a/b/Script.kts"] {
            let available = registry
                .language_for_file_path(Path::new(path))
                .unwrap_or_else(|| panic!("no language for {path}"));
            assert_eq!(available.name().as_ref(), "Kotlin", "{path}");
        }
        assert!(
            registry
                .language_for_file_path(Path::new("Main.xyz"))
                .is_none()
        );

        let language = registry.language_for_name("Kotlin").await.unwrap();
        assert_eq!(language.name().as_ref(), "Kotlin");
        assert!(language.grammar().is_some(), "Kotlin loads with a grammar");
    }

    #[gpui::test]
    async fn registering_twice_keeps_one_kotlin(cx: &mut TestAppContext) {
        let registry = registry(cx);
        init(&registry);
        let kotlin = registry
            .language_names()
            .into_iter()
            .filter(|name| name.as_ref() == "Kotlin")
            .count();
        assert_eq!(kotlin, 1);
    }

    fn captures(
        grammar: impl Into<tree_sitter::Language> + Copy,
        config: LanguageConfig,
        queries: LanguageQueries,
        highlights: &str,
        snippet: &str,
    ) -> BTreeSet<(String, String)> {
        let language = Language::new(config, Some(grammar.into()))
            .with_queries(queries)
            .expect("every query compiles against the grammar");
        assert!(language.grammar().is_some());

        let ts_language: tree_sitter::Language = grammar.into();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&ts_language)
            .expect("the grammar ABI loads");
        let tree = parser.parse(snippet, None).unwrap();
        assert!(!tree.root_node().has_error());

        let query =
            tree_sitter::Query::new(&ts_language, highlights).expect("highlights.scm compiles");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut captured = BTreeSet::new();
        let mut matches = cursor.matches(&query, tree.root_node(), snippet.as_bytes());
        while let Some(found) = matches.next() {
            for capture in found.captures {
                let name = query.capture_names()[capture.index as usize].to_string();
                captured.insert((name, snippet[capture.node.byte_range()].to_string()));
            }
        }
        captured
    }

    fn assert_captured(captured: &BTreeSet<(String, String)>, expected: &[(&str, &str)]) {
        for (prefix, text) in expected {
            assert!(
                captured
                    .iter()
                    .any(|(name, found)| name.starts_with(prefix) && found == text),
                "missing {prefix} {text:?} in {captured:?}"
            );
        }
    }

    #[test]
    fn highlight_query_captures_a_snippet() {
        let captured = captures(
            tree_sitter_kotlin::LANGUAGE,
            parse_config(KOTLIN_CONFIG),
            kotlin_queries(),
            include_str!("kotlin/highlights.scm"),
            SNIPPET,
        );
        assert_captured(
            &captured,
            &[
                ("keyword", "fun"),
                ("keyword", "val"),
                ("number", "1"),
                ("function", "main"),
            ],
        );
    }

    #[gpui::test]
    async fn java_selected_for_java_files(cx: &mut TestAppContext) {
        let registry = registry(cx);
        let available = registry
            .language_for_file_path(Path::new("src/Main.java"))
            .expect("no language for Main.java");
        assert_eq!(available.name().as_ref(), "Java");

        let language = registry.language_for_name("Java").await.unwrap();
        assert!(language.grammar().is_some(), "Java loads with a grammar");
    }

    #[test]
    fn java_highlight_query_captures_a_snippet() {
        const JAVA_SNIPPET: &str = "public class Main { int count = 1; void run() { return; } }";
        let captured = captures(
            tree_sitter_java::LANGUAGE,
            parse_config(JAVA_CONFIG),
            java_queries(),
            include_str!("java/highlights.scm"),
            JAVA_SNIPPET,
        );
        assert_captured(
            &captured,
            &[
                ("keyword", "class"),
                ("keyword", "return"),
                ("number", "1"),
                ("function", "run"),
                ("type", "Main"),
            ],
        );
    }

    #[gpui::test]
    async fn toml_selected_for_toml_files(cx: &mut TestAppContext) {
        let registry = registry(cx);
        for path in ["Cargo.toml", "Cargo.lock", "a/b/pyproject.toml"] {
            let available = registry
                .language_for_file_path(Path::new(path))
                .unwrap_or_else(|| panic!("no language for {path}"));
            assert_eq!(available.name().as_ref(), "TOML", "{path}");
        }

        let language = registry.language_for_name("TOML").await.unwrap();
        assert!(language.grammar().is_some(), "TOML loads with a grammar");
    }

    #[test]
    fn toml_highlight_query_captures_a_snippet() {
        const TOML_SNIPPET: &str = "[package]\nname = \"bongos\"\nversion = 1\n# note\n";
        let captured = captures(
            tree_sitter_toml_ng::LANGUAGE,
            parse_config(TOML_CONFIG),
            toml_queries(),
            include_str!("toml/highlights.scm"),
            TOML_SNIPPET,
        );
        assert_captured(
            &captured,
            &[
                ("string", "\"bongos\""),
                ("number", "1"),
                ("comment", "# note"),
            ],
        );
        assert!(
            captured.iter().any(|(name, _)| name == "property"),
            "missing property capture in {captured:?}"
        );
    }
}
