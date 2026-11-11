//! Definition layers: shipped, profile and project.
//!
//! A definition is a directory holding `workflow.toml`. [`resolve`] scans the three layer
//! directories and picks one definition per `name` (the key in the file, not the directory name):
//! project over profile over shipped.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const DEFINITION_FILE: &str = "workflow.toml";

/// Ordered from the lowest to the highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    Shipped,
    Profile,
    Project,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Shipped => "shipped",
            Layer::Profile => "profile",
            Layer::Project => "project",
        }
    }
}

/// The directory of each layer; `None` when the layer does not exist, as for a one-off task with no
/// project layer.
#[derive(Debug, Clone, Default)]
pub struct LayerDirs {
    pub shipped: Option<PathBuf>,
    pub profile: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

impl LayerDirs {
    fn layers(&self) -> [(Layer, Option<&Path>); 3] {
        [
            (Layer::Shipped, self.shipped.as_deref()),
            (Layer::Profile, self.profile.as_deref()),
            (Layer::Project, self.project.as_deref()),
        ]
    }
}

/// The definition a name resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub layer: Layer,
    pub dir: PathBuf,
    /// The lower layers that also define this name, which this definition shadows.
    pub overrides: Vec<Layer>,
}

/// Two or more definitions with one name in the same layer. All of them are refused, so the name
/// resolves to a lower layer or to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    pub name: String,
    pub layer: Layer,
    pub dirs: Vec<PathBuf>,
}

impl Duplicate {
    pub fn message(&self) -> String {
        let dirs = self
            .dirs
            .iter()
            .filter_map(|dir| dir.file_name())
            .map(|name| format!("{}/", name.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("duplicate name in {}: {dirs}", self.layer.label())
    }
}

/// A directory in a layer that could not be read as a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub layer: Layer,
    pub dir: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// One entry per name, sorted by name.
    pub resolved: Vec<Resolved>,
    pub duplicates: Vec<Duplicate>,
    pub problems: Vec<Problem>,
}

impl Resolution {
    pub fn get(&self, name: &str) -> Option<&Resolved> {
        self.resolved.iter().find(|resolved| resolved.name == name)
    }
}

/// A definition directory found in a layer, before names are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub layer: Layer,
    pub dir: PathBuf,
    pub name: String,
}

/// Scans the layer directories and resolves every name. A missing layer directory is an empty
/// layer; an unreadable definition is reported in [`Resolution::problems`] and skipped.
pub fn resolve(dirs: &LayerDirs) -> Resolution {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for (layer, dir) in dirs.layers() {
        if let Some(dir) = dir {
            scan(layer, dir, &mut entries, &mut problems);
        }
    }
    let mut resolution = resolve_entries(entries);
    resolution.problems = problems;
    resolution
}

/// Resolves already-scanned entries: project over profile over shipped, duplicates reported.
pub fn resolve_entries(entries: Vec<Entry>) -> Resolution {
    let mut by_name: BTreeMap<String, BTreeMap<Layer, Vec<PathBuf>>> = BTreeMap::new();
    for entry in entries {
        by_name
            .entry(entry.name)
            .or_default()
            .entry(entry.layer)
            .or_default()
            .push(entry.dir);
    }

    let mut resolution = Resolution::default();
    for (name, layers) in by_name {
        let mut candidates = Vec::new();
        for (layer, mut dirs) in layers {
            dirs.sort();
            if dirs.len() > 1 {
                resolution.duplicates.push(Duplicate {
                    name: name.clone(),
                    layer,
                    dirs,
                });
            } else if let Some(dir) = dirs.pop() {
                candidates.push((layer, dir));
            }
        }
        let Some((layer, dir)) = candidates.pop() else {
            continue;
        };
        resolution.resolved.push(Resolved {
            name,
            layer,
            dir,
            overrides: candidates.into_iter().map(|(layer, _)| layer).collect(),
        });
    }
    resolution
}

fn scan(layer: Layer, root: &Path, entries: &mut Vec<Entry>, problems: &mut Vec<Problem>) {
    let listing = match fs::read_dir(root) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            problems.push(Problem {
                layer,
                dir: root.to_path_buf(),
                message: format!("cannot read {}: {error}", root.display()),
            });
            return;
        }
    };

    let mut dirs = Vec::new();
    for item in listing {
        match item {
            Ok(item) => {
                if item.path().is_dir() {
                    dirs.push(item.path());
                }
            }
            Err(error) => problems.push(Problem {
                layer,
                dir: root.to_path_buf(),
                message: format!("cannot read {}: {error}", root.display()),
            }),
        }
    }
    dirs.sort();

    for dir in dirs {
        match read_name(&dir) {
            Ok(Some(name)) => entries.push(Entry { layer, dir, name }),
            Ok(None) => {}
            Err(message) => problems.push(Problem {
                layer,
                dir,
                message,
            }),
        }
    }
}

/// Reads the `name` key of a definition directory. A directory with no `workflow.toml` is not a
/// definition and gives `None`.
fn read_name(dir: &Path) -> Result<Option<String>, String> {
    let file = dir.join(DEFINITION_FILE);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", file.display())),
    };
    let table: toml::Table = toml::from_str(&text)
        .map_err(|error| format!("{}: {}", file.display(), error.message()))?;
    match table.get("name").and_then(toml::Value::as_str) {
        Some(name) if !name.trim().is_empty() => Ok(Some(name.to_string())),
        _ => Err(format!("{} has no `name`", file.display())),
    }
}
