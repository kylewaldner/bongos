//! The `Prompts` language: the major mode of `prompts.txt` files (design D5 and D6). It has no
//! grammar. Its editing defaults, which save a file exactly as typed, live under `languages` in
//! `default.json`.

use std::sync::Arc;

use gpui::{App, Entity};
use language::{Buffer, LanguageConfig, LanguageMatcher, LanguageRegistry, LoadedLanguage};
use project::Project;

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

/// Gives `buffer` the `Prompts` language unless it has it already, which makes the prompts mode
/// attach to a prompts file that is not called `prompts.txt` (a file picked with `prompts: use
/// file`). The language is set once it has loaded.
pub(crate) fn apply(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) {
    let has_it = buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == NAME);
    if has_it {
        return;
    }
    let languages = project.read(cx).languages().clone();
    let (project, buffer) = (project.clone(), buffer.clone());
    cx.spawn(
        async move |cx| match languages.language_for_name(NAME).await {
            Ok(language) => project.update(cx, |project, cx| {
                project.set_language_for_buffer(&buffer, language, cx)
            }),
            Err(error) => log::warn!("prompts: no Prompts language: {error}"),
        },
    )
    .detach();
}
