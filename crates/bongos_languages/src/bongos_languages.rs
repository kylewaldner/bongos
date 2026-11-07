//! Languages that bongos ships built in, beyond the ones inherited with the editor.
//!
//! The Kotlin config and queries in `src/kotlin/` come from the Zed Kotlin extension
//! (github.com/zed-extensions/kotlin, MIT, see `LICENSE-KOTLIN-EXTENSION`). The grammar is
//! fwcd/tree-sitter-kotlin (MIT) at the commit that extension pins.
//!
//! Everything here is compiled into the binary: the grammar is linked in and the queries are embedded,
//! so nothing is fetched at runtime.

use std::sync::Arc;

use language::{LanguageConfig, LanguageQueries, LanguageRegistry, LoadedLanguage};

const KOTLIN_CONFIG: &str = include_str!("kotlin/config.toml");

/// Registers the built-in languages and their grammars with the registry.
///
/// Registering a language under a name that is already registered replaces it, so a Kotlin
/// language provided later by an extension never produces a second "Kotlin" entry.
pub fn init(languages: &LanguageRegistry) {
    languages.register_native_grammars([("kotlin", tree_sitter_kotlin::LANGUAGE)]);

    let config = kotlin_config();
    languages.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(move || {
            Ok(LoadedLanguage {
                config: kotlin_config(),
                queries: kotlin_queries(),
                context_provider: None,
                toolchain_provider: None,
                manifest_name: None,
            })
        }),
    );
}

fn kotlin_config() -> LanguageConfig {
    toml::from_str(KOTLIN_CONFIG).expect("the embedded Kotlin config.toml is valid")
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
                .language_for_file_path(Path::new("Main.java"))
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

    #[test]
    fn highlight_query_captures_a_snippet() {
        let language = Language::new(kotlin_config(), Some(tree_sitter_kotlin::LANGUAGE.into()))
            .with_queries(kotlin_queries())
            .expect("every Kotlin query compiles against the grammar");
        assert!(language.grammar().is_some());

        let ts_language: tree_sitter::Language = tree_sitter_kotlin::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&ts_language)
            .expect("the grammar ABI loads");
        let tree = parser.parse(SNIPPET, None).unwrap();
        assert!(!tree.root_node().has_error());

        let query = tree_sitter::Query::new(&ts_language, include_str!("kotlin/highlights.scm"))
            .expect("highlights.scm compiles");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut captured = BTreeSet::new();
        let mut matches = cursor.matches(&query, tree.root_node(), SNIPPET.as_bytes());
        while let Some(found) = matches.next() {
            for capture in found.captures {
                let name = query.capture_names()[capture.index as usize].to_string();
                captured.insert((name, &SNIPPET[capture.node.byte_range()]));
            }
        }
        for expected in [("keyword", "fun"), ("keyword", "val"), ("number", "1")] {
            assert!(
                captured
                    .iter()
                    .any(|(n, t)| n.starts_with(expected.0) && *t == expected.1),
                "missing {expected:?} in {captured:?}"
            );
        }
        assert!(
            captured
                .iter()
                .any(|(n, t)| n.starts_with("function") && *t == "main"),
            "missing function capture for main in {captured:?}"
        );
    }
}
