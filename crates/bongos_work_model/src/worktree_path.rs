//! Where a task's git worktree goes: parsing the `worktree_root` templates of the settings,
//! turning one into a path, and keeping that path short enough to build in.
//!
//! The module is pure. It never reads the file system or the environment: the home directory, the
//! central directory and the repository path all come in as arguments, so the same inputs give
//! the same path on every machine and in every test.
//!
//! A template is a path with two tokens, `{repo}` and `{slug}`. Both are filled with a single
//! sanitized path component, so a repository or task name can never add or remove a directory
//! level or climb out of the root.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// The longest slug that goes into a worktree path before [`path_slug`] shortens it. A task's own
/// slug, directory and branch keep the full slug; only the worktree folder is cut.
pub const MAX_PATH_SLUG: usize = 48;

/// The longest worktree path, in characters of its lossy string form. Long paths break build
/// tools and socket paths, so past this [`resolve`] shortens the slug and then gives up.
pub const MAX_WORKTREE_PATH: usize = 200;

/// The shortest slug limit [`resolve`] falls back to while looking for a path that fits.
const MIN_PATH_SLUG: usize = 15;

/// How much [`resolve`] lowers the slug limit at each try.
const SLUG_LIMIT_STEP: usize = 3;

/// The length of the hash suffix [`path_slug`] appends, in hex digits.
const SUFFIX_DIGITS: usize = 6;

/// A cut slug is not shorter than this before the suffix, else it is cut hard instead.
const MIN_CUT_LEN: usize = 8;

const EMPTY_REPO: &str = "repository";
const EMPTY_SLUG: &str = "task";

/// The slug shown in [`preview`] where the task's slug will go.
const PREVIEW_SLUG: &str = "<task>";

/// The setting that names the central layout when it is too long: `null` there means central, so
/// shortening the default root is how a user replaces it.
const DEFAULT_SETTING: &str = "work.default_worktree_root";

/// A parsed `worktree_root` template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Repo,
    Slug,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("the worktree root is empty")]
    Empty,
    #[error("the worktree root has an unknown token {{{0}}}; use {{repo}} or {{slug}}")]
    UnknownToken(String),
    #[error("the worktree root has an unbalanced brace")]
    Malformed,
    #[error("the worktree root has a `..` component")]
    ParentComponent,
}

/// Which settings layer a template came from. The resolver cannot tell by itself (it only sees
/// the winning text), so the caller says: the repository's own `worktree_root`, or the default
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateLayer {
    Own,
    Default,
}

/// A template as written in settings, with the layer it came from.
#[derive(Debug, Clone, Copy)]
pub struct Configured<'a> {
    pub text: &'a str,
    pub layer: TemplateLayer,
}

/// Where a resolved path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The repository's own `worktree_root`.
    Own,
    /// `work.default_worktree_root`.
    Default,
    /// No template: the central directory.
    Central,
}

pub struct ResolveInput<'a> {
    /// The winning template, or `None` for the central layout `<central_dir>/{repo}/{slug}`.
    pub template: Option<Configured<'a>>,
    pub repository_name: &'a str,
    /// The repository path as written in settings: `~` is expanded, nothing is canonicalized.
    pub repository_path: &'a Path,
    /// The task's full slug.
    pub slug: &'a str,
    /// The task id; its hash makes a shortened slug unique.
    pub task_id: &'a str,
    pub home: &'a Path,
    pub central_dir: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResolveError {
    #[error(transparent)]
    Template(#[from] TemplateError),
    /// Even the shortest slug leaves the path over [`MAX_WORKTREE_PATH`]; only the root can
    /// change. `setting` names the setting to shorten.
    #[error(
        "The worktree path {} is longer than {MAX_WORKTREE_PATH} characters; shorten {setting}",
        path.display()
    )]
    TooLong { path: PathBuf, setting: String },
}

/// Parses a template. A template without `{slug}` gets `/{slug}` appended, so a worktree never
/// lands on the root itself.
pub fn parse(template: &str) -> Result<Template, TemplateError> {
    if template.trim().is_empty() {
        return Err(TemplateError::Empty);
    }
    if template
        .split(['/', '\\'])
        .any(|component| component == "..")
    {
        return Err(TemplateError::ParentComponent);
    }
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut characters = template.chars();
    while let Some(character) = characters.next() {
        match character {
            '{' => {
                let mut name = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some('{') | None => return Err(TemplateError::Malformed),
                        Some(other) => name.push(other),
                    }
                }
                let part = match name.as_str() {
                    "repo" => Part::Repo,
                    "slug" => Part::Slug,
                    _ => return Err(TemplateError::UnknownToken(name)),
                };
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(part);
            }
            '}' => return Err(TemplateError::Malformed),
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    if !parts.contains(&Part::Slug) {
        let ends_with_separator =
            matches!(parts.last(), Some(Part::Literal(text)) if text.ends_with('/'));
        if !ends_with_separator {
            parts.push(Part::Literal("/".to_string()));
        }
        parts.push(Part::Slug);
    }
    Ok(Template { parts })
}

/// The worktree path for a task, or why there is none. The slug is shortened by [`path_slug`]
/// when needed, and further while the path is over [`MAX_WORKTREE_PATH`].
pub fn resolve(input: &ResolveInput) -> Result<Resolved, ResolveError> {
    let template = input
        .template
        .map(|configured| parse(configured.text))
        .transpose()?;
    let source = match input.template {
        Some(Configured {
            layer: TemplateLayer::Own,
            ..
        }) => Source::Own,
        Some(Configured {
            layer: TemplateLayer::Default,
            ..
        }) => Source::Default,
        None => Source::Central,
    };
    let place = Place {
        template: template.as_ref(),
        repository_name: input.repository_name,
        repository_path: input.repository_path,
        home: input.home,
        central_dir: input.central_dir,
    };

    let mut limit = MAX_PATH_SLUG;
    loop {
        let slug = path_slug(input.slug, input.task_id, limit);
        let path = place.path(&slug);
        if path.to_string_lossy().chars().count() <= MAX_WORKTREE_PATH {
            return Ok(Resolved { path, source });
        }
        if limit <= MIN_PATH_SLUG {
            let setting = if source == Source::Own {
                format!("work.repositories[{}].worktree_root", input.repository_name)
            } else {
                DEFAULT_SETTING.to_string()
            };
            return Err(ResolveError::TooLong { path, setting });
        }
        limit = limit.saturating_sub(SLUG_LIMIT_STEP).max(MIN_PATH_SLUG);
    }
}

/// The path for the screen, with the slug shown as `<task>`. It is not checked against
/// [`MAX_WORKTREE_PATH`]: that needs a real task.
pub fn preview(
    template: Option<&str>,
    repository_name: &str,
    repository_path: &Path,
    home: &Path,
    central_dir: &Path,
) -> Result<String, TemplateError> {
    let template = template.map(parse).transpose()?;
    let place = Place {
        template: template.as_ref(),
        repository_name,
        repository_path,
        home,
        central_dir,
    };
    Ok(place.path(PREVIEW_SLUG).to_string_lossy().into_owned())
}

/// The slug as it appears in a worktree path. Within `limit` characters it is unchanged. Longer,
/// it is cut at the last `-` at or before `limit - 7` (a hard cut at that length when the cut
/// would leave fewer than 8 characters) and `-` plus six hex digits of the FNV-1a hash of the
/// task id follow, so two long slugs with one prefix still get different folders.
pub fn path_slug(slug: &str, task_id: &str, limit: usize) -> String {
    if slug.chars().count() <= limit {
        return slug.to_string();
    }
    let room = limit.saturating_sub(SUFFIX_DIGITS + 1);
    let dash_cut = slug
        .char_indices()
        .take(room + 1)
        .filter(|(_, character)| *character == '-')
        .map(|(index, _)| index)
        .last()
        .filter(|&index| slug[..index].chars().count() >= MIN_CUT_LEN);
    let stem = match dash_cut {
        Some(index) => &slug[..index],
        None => {
            let end = slug
                .char_indices()
                .nth(room)
                .map_or(slug.len(), |(index, _)| index);
            slug[..end].trim_end_matches('-')
        }
    };
    format!("{stem}-{}", id_suffix(task_id))
}

/// The first six hex digits of the 32-bit FNV-1a hash of the id. It is written out here because
/// `DefaultHasher` may change between Rust versions and the suffix must stay put: a worktree's
/// folder name is derived again every time the task is looked at.
fn id_suffix(task_id: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in task_id.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")[..SUFFIX_DIGITS].to_string()
}

/// One path component from a token value: `/` and `\` become `-`, so do leading dots (no hidden
/// folder, no `.` or `..`), and nothing at all becomes `empty`.
fn sanitize(value: &str, empty: &str) -> String {
    let mut leading = true;
    let component: String = value
        .chars()
        .map(|character| {
            if leading && character == '.' {
                return '-';
            }
            leading = false;
            if matches!(character, '/' | '\\') {
                '-'
            } else {
                character
            }
        })
        .collect();
    if component.trim().is_empty() {
        empty.to_string()
    } else {
        component
    }
}

/// Everything a path depends on besides the slug.
struct Place<'a> {
    template: Option<&'a Template>,
    repository_name: &'a str,
    repository_path: &'a Path,
    home: &'a Path,
    central_dir: &'a Path,
}

impl Place<'_> {
    fn path(&self, slug: &str) -> PathBuf {
        let repository = sanitize(self.repository_name, EMPTY_REPO);
        let slug = sanitize(slug, EMPTY_SLUG);
        let Some(template) = self.template else {
            return normalize(&self.central_dir.join(repository).join(slug));
        };

        // Only a `~` the user wrote at the very start is the home directory; a repository named
        // `~` filled in by a token is just a name.
        let mut parts = template.parts.iter();
        let mut text = String::new();
        let mut home_relative = false;
        if let Some(Part::Literal(first)) = template.parts.first() {
            if first == "~" || first.starts_with("~/") {
                home_relative = true;
                parts.next();
                text.push_str(&first[1..]);
            }
        }
        for part in parts {
            match part {
                Part::Literal(literal) => text.push_str(literal),
                Part::Repo => text.push_str(&repository),
                Part::Slug => text.push_str(&slug),
            }
        }

        let relative = text.trim_start_matches('/');
        let path = if home_relative {
            self.home.join(relative)
        } else if text.starts_with('/') {
            PathBuf::from(&text)
        } else {
            self.repository_parent().join(&text)
        };
        normalize(&path)
    }

    /// The directory a relative template is relative to: the parent of the repository path, with
    /// `~` expanded and nothing resolved on disk.
    fn repository_parent(&self) -> PathBuf {
        let repository = match self.repository_path.strip_prefix("~") {
            Ok(rest) => self.home.join(rest),
            Err(_) => self.repository_path.to_path_buf(),
        };
        repository
            .parent()
            .map_or_else(|| repository.clone(), Path::to_path_buf)
    }
}

/// The path with repeated separators and `.` components removed. `..` stays: templates cannot
/// hold one, and a repository path is taken as written.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}
