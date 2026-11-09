//! The `Proposal` language: the major mode of `<n>.proposal.txt` files. It has no grammar. Its
//! editing defaults, which save a file exactly as typed, live under `languages` in `default.json`.

use std::sync::Arc;

use language::{LanguageConfig, LanguageMatcher, LanguageRegistry, LoadedLanguage};

/// The name of the language.
pub const NAME: &str = "Proposal";

/// The file name suffix that selects the language. The registry matches a suffix when the file
/// name ends with `.<suffix>` and prefers the longer match, so this beats Plain Text's `txt`.
pub const FILE_SUFFIX: &str = "proposal.txt";

fn config() -> LanguageConfig {
    LanguageConfig {
        name: NAME.into(),
        matcher: LanguageMatcher {
            path_suffixes: vec![FILE_SUFFIX.to_string()],
            ..LanguageMatcher::default()
        },
        ..LanguageConfig::default()
    }
}

/// Registers the `Proposal` language with the registry. Registering it again replaces it.
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
