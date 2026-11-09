//! The `Prompts` language: the major mode of `prompts.txt` files (design D5 and D6). It has no
//! grammar. Its editing defaults, which save a file exactly as typed, live under `languages` in
//! `default.json`.

use std::sync::Arc;

use language::{LanguageConfig, LanguageMatcher, LanguageRegistry, LoadedLanguage};

/// The name of the language.
pub const NAME: &str = "Prompts";

/// The file name that selects the language. The registry matches a suffix against the whole file
/// name as well as the extension and prefers the longer match, so this beats Plain Text's `txt`.
pub const FILE_NAME: &str = "prompts.txt";

fn config() -> LanguageConfig {
    LanguageConfig {
        name: NAME.into(),
        matcher: LanguageMatcher {
            path_suffixes: vec![FILE_NAME.to_string()],
            ..LanguageMatcher::default()
        },
        ..LanguageConfig::default()
    }
}

/// Registers the `Prompts` language with the registry. Registering it again replaces it.
pub fn register_language(languages: &LanguageRegistry) {
    let config = config();
    languages.register_language(
        config.name.clone(),
        None,
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(|| {
            Ok(LoadedLanguage {
                config: self::config(),
                queries: Default::default(),
                context_provider: None,
                toolchain_provider: None,
                manifest_name: None,
            })
        }),
    );
}
