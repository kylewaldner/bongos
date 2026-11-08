//! Session types: one directory per type under the types root (`type.toml`, `context.md`, optional
//! per-agent files), the skills a type lists, seeding of the shipped defaults, and resolving a type
//! for an agent. A type is read from disk at every resolve, so an edit applies to the next chat.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bongos_work_model::files::write_atomic;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::record::{Agent, Extra, Layout, parse};
use crate::shipped::SHIPPED_TYPES;

/// The directory under the types root that holds skills a type can list. It is not a type.
pub const SKILLS_DIR: &str = "skills";

const TYPE_FILE: &str = "type.toml";
const CONTEXT_FILE: &str = "context.md";
const SKILL_FILE: &str = "SKILL.md";

fn default_true() -> bool {
    true
}

/// `type.toml`. Keys this version does not know are kept in `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeConfig {
    pub schema: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub layout: Layout,
    /// Skill names, resolved by [`resolve`].
    #[serde(default)]
    pub skills: Vec<String>,
    /// Task file, notes directory, worktree and dependencies are injected.
    #[serde(default)]
    pub task_context: bool,
    /// PR links from the task's result are injected.
    #[serde(default)]
    pub review_context: bool,
    /// The session is told the task is in thinking mode.
    #[serde(default)]
    pub thinking: bool,
    /// The shared artifact list is injected.
    #[serde(default = "default_true")]
    pub artifacts: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `claude.toml` or `codex.toml`: what a type changes for one agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentOverrides {
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A skill found for a type: where it was found and the text of its `SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    pub dir: PathBuf,
    pub text: String,
}

/// A type read from disk for one agent.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedType {
    pub name: String,
    pub dir: PathBuf,
    pub config: TypeConfig,
    /// `context.md`; empty when the file is missing.
    pub context: String,
    /// `claude.md` or `codex.md` for the agent asked for, appended after `context`.
    pub agent_context: Option<String>,
    /// `claude.toml` or `codex.toml` for the agent asked for; default when absent.
    pub overrides: AgentOverrides,
    /// The listed skills that were found, in the order listed.
    pub skills: Vec<ResolvedSkill>,
    /// The listed skills that were not found. A missing skill never stops a launch.
    pub missing_skills: Vec<String>,
}

/// Parses the text of a `type.toml`. `path` is only named in errors.
pub fn parse_type_config(text: &str, path: Option<&Path>) -> Result<TypeConfig> {
    parse(text, path)
}

/// A type or skill name is one plain path component. `skills` is reserved for the skills
/// directory, and names starting with a dot are never types.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name != SKILLS_DIR
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}

/// Writes each shipped type whose directory does not exist under `types_root`, creating the root
/// as needed. An existing directory is left exactly as it is, whatever it holds, so a file Kyle
/// edited or added, or one he deleted, is never touched. A directory is only ever written by the
/// call that created it (`create_dir` fails when it exists), so nothing is replaced. Returns the
/// names of the types written.
pub fn seed(types_root: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(types_root).map_err(|error| Error::io(types_root, error))?;
    let mut written = Vec::new();
    for shipped in SHIPPED_TYPES {
        let dir = types_root.join(shipped.name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::io(&dir, error)),
        }
        for (file_name, text) in shipped.files {
            let path = dir.join(file_name);
            write_atomic(&path, text.as_bytes()).map_err(|error| Error::io(&path, error))?;
        }
        written.push(shipped.name.to_string());
    }
    Ok(written)
}

/// Reads the type `chat_type` under `types_root` for `agent`, fresh from disk, and looks up its
/// skills: `<types_root>/skills/<name>/SKILL.md` first, then `<claude_home>/skills/<name>/SKILL.md`
/// (`claude_home` is the `.claude` directory). A skill found in neither place is listed in
/// `missing_skills` and is not an error.
pub fn resolve(
    chat_type: &str,
    agent: Agent,
    types_root: &Path,
    claude_home: &Path,
) -> Result<ResolvedType> {
    if !valid_name(chat_type) {
        return Err(Error::new(ErrorKind::InvalidName(chat_type.to_string())));
    }
    let dir = types_root.join(chat_type);
    let config_path = dir.join(TYPE_FILE);
    let text = match fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let missing = !dir.is_dir();
            return Err(if missing {
                Error::new(ErrorKind::NoSuchType(chat_type.to_string())).with_path(Some(&dir))
            } else {
                Error::io(&config_path, error)
            });
        }
        Err(error) => return Err(Error::io(&config_path, error)),
    };
    let config = parse_type_config(&text, Some(&config_path))?;

    let context = read_optional(&dir.join(CONTEXT_FILE))?.unwrap_or_default();
    let agent_context = read_optional(&dir.join(format!("{}.md", agent.as_str())))?;
    let overrides_path = dir.join(format!("{}.toml", agent.as_str()));
    let overrides = match read_optional(&overrides_path)? {
        Some(text) => toml::from_str(&text)
            .map_err(|error| Error::syntax(&text, &error).with_path(Some(&overrides_path)))?,
        None => AgentOverrides::default(),
    };

    let mut skills = Vec::new();
    let mut missing_skills = Vec::new();
    for name in &config.skills {
        match find_skill(name, types_root, claude_home)? {
            Some(skill) => skills.push(skill),
            None => missing_skills.push(name.clone()),
        }
    }

    Ok(ResolvedType {
        name: chat_type.to_string(),
        dir,
        config,
        context,
        agent_context,
        overrides,
        skills,
        missing_skills,
    })
}

fn find_skill(name: &str, types_root: &Path, claude_home: &Path) -> Result<Option<ResolvedSkill>> {
    if !valid_skill_name(name) {
        return Ok(None);
    }
    for root in [types_root.join(SKILLS_DIR), claude_home.join("skills")] {
        let dir = root.join(name);
        if let Some(text) = read_optional(&dir.join(SKILL_FILE))? {
            return Ok(Some(ResolvedSkill {
                name: name.to_string(),
                dir,
                text,
            }));
        }
    }
    Ok(None)
}

/// The text of a file, or `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(path, error)),
    }
}
