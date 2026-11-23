//! Where a lane's trees and named directories go, and the check that no two lanes share one.
//!
//! Locations combine in layers, last wins: the user's worktree settings (a repository's own
//! `worktree_root`, `work.default_worktree_root`, else the central directory) with the slug
//! `lane-<lane>`; the project's `[locations]` in its lane table; the project profile; and the
//! lane binding's own `location` on a tree. Named directories default to
//! `<project folder>/lanes/<lane>/<name>`. Each location carries the root it may be made below
//! and the layer that set it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use bongos_lanes_model::table::{Binding, LaneTable, Locations, TreeMode};
use bongos_lanes_model::{LaneId, Problem};
use bongos_work_model::worktree_path::{
    self, Configured, ResolveInput, Source, TemplateLayer, fixed_root,
};

use crate::create::lexical;
use crate::profile::LaneProfileSource;

/// One repository a lane may bind a tree of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryPlace {
    pub name: String,
    /// The primary checkout, with `~` expanded.
    pub path: PathBuf,
    /// The winning worktree root template of the user's settings, with its layer; `None` for
    /// the central directory.
    pub template: Option<(String, TemplateLayer)>,
}

/// Everything besides the lane table and the profile that decides where a lane's locations go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationInputs {
    pub project_id: String,
    /// The project's folder on this machine; named directories default to below it.
    pub project_folder: PathBuf,
    /// The project's own repository (its definitions), which no location may be inside.
    pub project_repository: Option<PathBuf>,
    pub home: PathBuf,
    /// The central worktree directory of the user's settings.
    pub central: PathBuf,
    pub repositories: Vec<RepositoryPlace>,
}

impl LocationInputs {
    /// The inputs of `project_id` from the app: its folder from the per-machine record of
    /// project locations, the repositories and worktree roots of the work settings, and the
    /// central directory.
    pub fn from_app(project_id: &str, cx: &gpui::App) -> Result<Self, String> {
        use settings::Settings as _;
        // A registered record wins; with none (the app registers none) the per-machine record of
        // project locations is read from disk.
        let location = cx
            .try_global::<bongos_project_repo::RepositoryLocations>()
            .and_then(|locations| locations.location(project_id).cloned())
            .or_else(|| {
                let path = bongos_work::WorkStore::global(cx)
                    .map(|store| store.read(cx).locations_path())
                    .unwrap_or_else(bongos_project_repo::RepositoryLocations::default_path);
                bongos_project_repo::RepositoryLocations::load(path)
                    .ok()?
                    .location(project_id)
                    .cloned()
            })
            .ok_or_else(|| format!("project {project_id} has no location on this machine"))?;
        let central = bongos_work::central_worktrees_dir(cx)
            .ok_or_else(|| "the central worktree directory is not known".to_string())?;
        let settings = bongos_work::WorkSettings::get_global(cx);
        let repositories = settings
            .repositories
            .iter()
            .map(|repository| RepositoryPlace {
                name: repository.name.clone(),
                path: bongos_work::expand_home(&repository.path),
                template: repository
                    .worktree_root
                    .clone()
                    .map(|text| (text, TemplateLayer::Own))
                    .or_else(|| {
                        settings
                            .default_worktree_root
                            .clone()
                            .map(|text| (text, TemplateLayer::Default))
                    }),
            })
            .collect();
        Ok(Self {
            project_id: project_id.to_string(),
            project_folder: location.folder,
            project_repository: Some(location.repository),
            home: util::paths::home_dir().clone(),
            central,
            repositories,
        })
    }
}

/// The layer that set a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// The user's worktree settings.
    Settings(Source),
    /// The default of a named directory: below the project folder.
    ProjectFolder,
    /// The project's `[locations]`.
    Project,
    /// The project profile, by its name.
    Profile { name: String },
    /// The lane binding's own `location`.
    Lane,
}

impl Layer {
    /// The source the repositories screen shows for a tree of this layer.
    pub fn source(&self) -> Option<Source> {
        match self {
            Layer::Settings(source) => Some(source.clone()),
            Layer::ProjectFolder => None,
            Layer::Project => Some(Source::Project),
            Layer::Profile { name } => Some(Source::Profile { name: name.clone() }),
            Layer::Lane => Some(Source::Lane),
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Layer::Settings(source) => write!(f, "the worktree settings ({})", source.label()),
            Layer::ProjectFolder => f.write_str("the project folder"),
            Layer::Project => f.write_str("the project's lanes.toml [locations]"),
            Layer::Profile { name } => write!(f, "project profile {name}"),
            Layer::Lane => f.write_str("the lane binding"),
        }
    }
}

/// What a location is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The lane tree of `repo`, or its second tree.
    Tree { repo: String, base: bool },
    /// The named directory `name` of the binding, or its second-tree copy `<name>-base`.
    Dir { name: String, base: bool },
}

/// Where the location goes when its root cannot be used and its layer allows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub path: PathBuf,
    pub root: PathBuf,
}

/// One resolved location of a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneLocation {
    /// The name the profile is asked for: `tree:app`, `tree:app:base`, `target`, `target-base`.
    pub name: String,
    pub place: Place,
    pub path: PathBuf,
    /// The existing directory only the components below which may be made.
    pub root: PathBuf,
    pub layer: Layer,
    /// The central path, for a tree of the user's own or default root.
    pub fallback: Option<Fallback>,
}

/// Every location of one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneLocations {
    pub lane: LaneId,
    pub items: Vec<LaneLocation>,
}

impl LaneLocations {
    pub fn get(&self, name: &str) -> Option<&LaneLocation> {
        self.items.iter().find(|item| item.name == name)
    }

    /// The values of `{dir.<name>}`: each named directory's path. For the second tree of `repo`
    /// (`base_dirs` its `base_tree.dirs`), a listed name gives its `<name>-base` path instead.
    pub fn dir_values(&self, base_dirs: &[String]) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        for item in &self.items {
            let Place::Dir { name, base } = &item.place else {
                continue;
            };
            if *base != base_dirs.contains(name) {
                continue;
            }
            values.insert(name.clone(), item.path.to_string_lossy().into_owned());
        }
        values
    }
}

/// The name of the location of `repo`'s lane tree, or of its second tree.
pub fn tree_name(repo: &str, base: bool) -> String {
    if base {
        format!("tree:{repo}:base")
    } else {
        format!("tree:{repo}")
    }
}

/// The slug of a lane's tree folder: `lane-<lane>`, or `lane-<lane>-base` for its second tree.
pub fn tree_slug(lane: &LaneId, base: bool) -> String {
    if base {
        format!("lane-{lane}-base")
    } else {
        format!("lane-{lane}")
    }
}

/// Resolves every location of `lane` (of kind binding `binding`). A tip tree is resolved for each
/// persistent tree, a second tree for each tree with `base_tree`; a directory for each name in
/// `dirs`, and a `<name>-base` one for each name in a `base_tree.dirs`. Nothing is derived from
/// a name the project did not list.
pub fn resolve_lane(
    inputs: &LocationInputs,
    locations: &Locations,
    binding: Option<&Binding>,
    lane: &LaneId,
    profile: Option<&dyn LaneProfileSource>,
) -> Result<LaneLocations, Vec<Problem>> {
    let mut resolver = Resolver {
        inputs,
        locations,
        lane,
        profile,
        problems: Vec::new(),
    };
    let mut items = Vec::new();
    if let Some(binding) = binding {
        for tree in &binding.trees {
            if tree.mode == TreeMode::Persistent {
                items.extend(resolver.tree(&tree.repo, false, tree.location.as_deref()));
            }
            if tree.base_tree.is_some() {
                items.extend(resolver.tree(&tree.repo, true, tree.location.as_deref()));
            }
        }
        for dir in &binding.dirs {
            items.push(resolver.dir(&dir.value, false));
        }
        let mut base_dirs: Vec<&str> = binding
            .trees
            .iter()
            .filter_map(|tree| tree.base_tree.as_ref())
            .flat_map(|base| base.dirs.iter().map(|dir| dir.value.as_str()))
            .collect();
        base_dirs.sort_unstable();
        base_dirs.dedup();
        for name in base_dirs {
            items.push(resolver.dir(name, true));
        }
    }
    for item in &items {
        resolver.check_outside_checkouts(item);
    }
    if resolver.problems.is_empty() {
        Ok(LaneLocations {
            lane: lane.clone(),
            items,
        })
    } else {
        Err(resolver.problems)
    }
}

/// The lanes of `table` at their effective counts (the profile's count when it sets one).
pub fn table_lane_ids(
    table: &LaneTable,
    project: &str,
    profile: Option<&dyn LaneProfileSource>,
) -> Vec<LaneId> {
    table
        .kinds
        .iter()
        .flat_map(|kind| {
            let count = profile
                .and_then(|profile| profile.count(project, &kind.name))
                .unwrap_or(kind.count);
            (1..=count).map(|index| LaneId::new(kind.name.clone(), index))
        })
        .collect()
}

/// The binding of `lane`'s kind in `table`.
pub fn binding_of<'a>(table: &'a LaneTable, lane: &LaneId) -> Option<&'a Binding> {
    table
        .kinds
        .iter()
        .find(|kind| kind.name == lane.kind)
        .and_then(|kind| kind.binding.as_ref())
}

/// Resolves the locations of every lane in `lanes` and checks that none overlap. Used at start,
/// on every count change and before a lane's locations are made.
pub fn check_lanes(
    inputs: &LocationInputs,
    table: &LaneTable,
    lanes: &[LaneId],
    profile: Option<&dyn LaneProfileSource>,
) -> Result<Vec<LaneLocations>, Vec<Problem>> {
    let mut resolved = Vec::new();
    let mut problems = Vec::new();
    for lane in lanes {
        match resolve_lane(
            inputs,
            &table.locations,
            binding_of(table, lane),
            lane,
            profile,
        ) {
            Ok(locations) => resolved.push(locations),
            Err(found) => problems.extend(found),
        }
    }
    if let Err(message) = check_overlap(&resolved) {
        problems.push(Problem {
            requirement: "every lane has its own trees and directories".to_string(),
            message,
            hint: Some(
                "give the lanes different locations in lanes.toml or the project profile"
                    .to_string(),
            ),
        });
    }
    if problems.is_empty() {
        Ok(resolved)
    } else {
        Err(problems)
    }
}

/// Refuses two locations of different lanes at one path, or one inside the other (compared by
/// whole path components on the normalized text), naming the pair. Within one lane only an
/// equal pair is refused: a lane may keep a directory inside its own tree.
pub fn check_overlap(lanes: &[LaneLocations]) -> Result<(), String> {
    let all: Vec<(&LaneId, &LaneLocation, PathBuf)> = lanes
        .iter()
        .flat_map(|lane| {
            lane.items
                .iter()
                .map(move |item| (&lane.lane, item, lexical(&item.path)))
        })
        .collect();
    for (index, (lane_a, item_a, path_a)) in all.iter().enumerate() {
        for (lane_b, item_b, path_b) in &all[index + 1..] {
            if path_a == path_b {
                return Err(format!(
                    "{lane_a} {} and {lane_b} {} resolve to {}",
                    item_a.name,
                    item_b.name,
                    path_a.display()
                ));
            }
            if lane_a == lane_b {
                continue;
            }
            let (outer, inner) = if path_b.starts_with(path_a) {
                ((lane_a, item_a, path_a), (lane_b, item_b, path_b))
            } else if path_a.starts_with(path_b) {
                ((lane_b, item_b, path_b), (lane_a, item_a, path_a))
            } else {
                continue;
            };
            return Err(format!(
                "{} {} at {} contains {} {} at {}",
                outer.0,
                outer.1.name,
                outer.2.display(),
                inner.0,
                inner.1.name,
                inner.2.display()
            ));
        }
    }
    Ok(())
}

struct Resolver<'a> {
    inputs: &'a LocationInputs,
    locations: &'a Locations,
    lane: &'a LaneId,
    profile: Option<&'a dyn LaneProfileSource>,
    problems: Vec<Problem>,
}

impl Resolver<'_> {
    fn problem(&mut self, name: &str, message: String) {
        self.problems.push(Problem {
            requirement: format!("{} {name} has a usable location", self.lane),
            message,
            hint: None,
        });
    }

    fn profile_layer(&self) -> Layer {
        let name = self
            .profile
            .and_then(|profile| profile.name(&self.inputs.project_id))
            .unwrap_or_else(|| "(unnamed)".to_string());
        Layer::Profile { name }
    }

    fn tree(&mut self, repo: &str, base: bool, own: Option<&str>) -> Option<LaneLocation> {
        let name = tree_name(repo, base);
        let Some(repository) = self
            .inputs
            .repositories
            .iter()
            .find(|repository| repository.name == repo)
        else {
            self.problem(
                &name,
                format!("the lane binds a tree of {repo}, which is not in work.repositories"),
            );
            return None;
        };
        let slug = tree_slug(self.lane, base);

        let at = |(path, root): (PathBuf, PathBuf), layer: Layer, fallback: Option<Fallback>| {
            LaneLocation {
                name: name.clone(),
                place: Place::Tree {
                    repo: repo.to_string(),
                    base,
                },
                path,
                root,
                layer,
                fallback,
            }
        };

        // The user's settings, falling back to the central directory.
        let central = self.from_template(repository, &slug, None);
        let mut chosen = match &repository.template {
            None => central.map(|spot| at(spot, Layer::Settings(Source::Central), None)),
            Some((template, layer)) => {
                let source = match layer {
                    TemplateLayer::Own => Source::Own,
                    TemplateLayer::Default => Source::Default,
                };
                let fallback = central.ok().map(|(path, root)| Fallback { path, root });
                self.from_template(repository, &slug, Some(template))
                    .map(|spot| at(spot, Layer::Settings(source), fallback))
            }
        };

        if let Some(root) = self.locations.worktree_root_of(repo) {
            chosen = self
                .from_template(repository, &slug, Some(&root.value))
                .map(|spot| at(spot, Layer::Project, None));
        }
        if let Some(root) = self
            .profile
            .and_then(|profile| profile.worktree_root(&self.inputs.project_id, repo))
        {
            chosen = self
                .from_template(repository, &slug, Some(&root))
                .map(|spot| at(spot, self.profile_layer(), None));
        }
        if let Some(path) = self.profile_location(&name) {
            chosen = path.map(|path| {
                let root = parent_of(&path);
                at((path, root), self.profile_layer(), None)
            });
        }
        if let Some(template) = own {
            chosen = self
                .from_template(repository, &slug, Some(template))
                .map(|spot| at(spot, Layer::Lane, None));
        }
        match chosen {
            Ok(location) => Some(location),
            Err(message) => {
                self.problem(&name, message);
                None
            }
        }
    }

    fn dir(&mut self, binding_name: &str, base: bool) -> LaneLocation {
        let name = if base {
            format!("{binding_name}-base")
        } else {
            binding_name.to_string()
        };
        let lane = self.lane.to_string();
        let place = Place::Dir {
            name: binding_name.to_string(),
            base,
        };
        let folder = &self.inputs.project_folder;
        let mut location = LaneLocation {
            name: name.clone(),
            place,
            path: folder.join("lanes").join(&lane).join(&name),
            root: folder.clone(),
            layer: Layer::ProjectFolder,
            fallback: None,
        };
        if let Some(root) = &self.locations.dirs_root {
            let root = self.expand(&root.value);
            location = LaneLocation {
                path: root.join(&lane).join(&name),
                root,
                layer: Layer::Project,
                ..location
            };
        }
        if let Some(path) = self.profile_location(&name) {
            match path {
                Ok(path) => {
                    location = LaneLocation {
                        root: parent_of(&path),
                        path,
                        layer: self.profile_layer(),
                        ..location
                    }
                }
                Err(message) => self.problem(&name, message),
            }
        }
        location
    }

    /// The profile's path for `name`: `None` when it sets none, an error when it is relative.
    fn profile_location(&self, name: &str) -> Option<Result<PathBuf, String>> {
        let path = self
            .profile?
            .location(&self.inputs.project_id, self.lane, name)?;
        Some(if path.is_absolute() {
            Ok(lexical(&path))
        } else {
            Err(format!(
                "the project profile gives {name} the relative path {}; it must be absolute",
                path.display()
            ))
        })
    }

    /// The path of a tree with `slug` under `template` (the central layout for `None`) and the
    /// root it is made below. The central directory is bongos's own, so its root is the
    /// directory that holds it.
    fn from_template(
        &self,
        repository: &RepositoryPlace,
        slug: &str,
        template: Option<&str>,
    ) -> Result<(PathBuf, PathBuf), String> {
        let lane = self.lane.to_string();
        let resolved = worktree_path::resolve(&ResolveInput {
            template: template.map(|text| Configured {
                text,
                layer: TemplateLayer::Own,
            }),
            repository_name: &repository.name,
            repository_path: &repository.path,
            slug,
            task_id: &lane,
            home: &self.inputs.home,
            central_dir: &self.inputs.central,
        })
        .map_err(|error| error.to_string())?;
        let root = match template {
            Some(template) => fixed_root(template, &repository.path, &self.inputs.home)
                .map_err(|error| error.to_string())?,
            None => parent_of(&self.inputs.central),
        };
        Ok((lexical(&resolved.path), lexical(&root)))
    }

    /// A `[locations]` path: `~` is the home directory, a relative path is below the project
    /// folder.
    fn expand(&self, text: &str) -> PathBuf {
        let path = if text == "~" {
            self.inputs.home.clone()
        } else if let Some(rest) = text.strip_prefix("~/") {
            self.inputs.home.join(rest)
        } else {
            self.inputs.project_folder.join(text)
        };
        lexical(&path)
    }

    fn check_outside_checkouts(&mut self, item: &LaneLocation) {
        let path = lexical(&item.path);
        let checkouts = self
            .inputs
            .repositories
            .iter()
            .map(|repository| (repository.name.clone(), lexical(&repository.path)))
            .chain(
                self.inputs
                    .project_repository
                    .iter()
                    .map(|path| ("the project repository".to_string(), lexical(path))),
            )
            .collect::<Vec<_>>();
        for (name, checkout) in checkouts {
            if path.starts_with(&checkout) {
                self.problem(
                    &item.name,
                    format!(
                        "{} resolves to {}, inside the checkout of {name}",
                        item.name,
                        path.display()
                    ),
                );
            } else if checkout.starts_with(&path) {
                self.problem(
                    &item.name,
                    format!(
                        "{} resolves to {}, which holds the checkout of {name}",
                        item.name,
                        path.display()
                    ),
                );
            }
        }
    }
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}
