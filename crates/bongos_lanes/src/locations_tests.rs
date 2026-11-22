//! Where lane locations resolve: the layers in order, the defaults, the names the profile is
//! asked for, and the overlap check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bongos_lanes_model::table::LaneTable;
use bongos_lanes_model::{EnvEdit, LaneId, Problem, parse};
use bongos_work_model::worktree_path::{Source, TemplateLayer};
use gpui::{App, AppContext as _, Task};

use crate::locations::{
    LaneLocations, Layer, LocationInputs, RepositoryPlace, binding_of, check_lanes, resolve_lane,
    table_lane_ids,
};
use crate::profile::LaneProfileSource;

pub(crate) const BUILD_TABLE: &str = r#"schema = 1

[[kind]]
name = "build"
count = 2
[kind.binding]
env = { BUILD_DIR = "{dir.target}", SCRATCH = "{dir.scratch}" }
dirs = ["target", "scratch"]
trees = [{ repo = "app", mode = "persistent", base_tree = { dirs = ["target"] } }]
floors = ["target"]
"#;

/// A fake project profile: answers from its maps and records every location name asked.
#[derive(Default)]
pub(crate) struct FakeProfile {
    pub(crate) name: Option<String>,
    /// `(lane, location name)` to path.
    pub(crate) locations: HashMap<(String, String), PathBuf>,
    pub(crate) worktree_root: Option<String>,
    pub(crate) env: Vec<EnvEdit>,
    pub(crate) floors: HashMap<String, u64>,
    pub(crate) counts: HashMap<String, u32>,
    /// Problems `verify_lane` resolves with, after a hop on the background executor.
    pub(crate) verify_problems: Option<Vec<Problem>>,
    pub(crate) asked: Mutex<Vec<String>>,
    pub(crate) verified: Mutex<u32>,
}

impl FakeProfile {
    pub(crate) fn at(mut self, lane: &str, name: &str, path: impl Into<PathBuf>) -> Self {
        self.locations
            .insert((lane.to_string(), name.to_string()), path.into());
        self
    }

    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl LaneProfileSource for FakeProfile {
    fn name(&self, _project: &str) -> Option<String> {
        self.name.clone()
    }

    fn count(&self, _project: &str, kind: &str) -> Option<u32> {
        self.counts.get(kind).copied()
    }

    fn location(&self, _project: &str, lane: &LaneId, name: &str) -> Option<PathBuf> {
        self.asked.lock().unwrap().push(name.to_string());
        self.locations
            .get(&(lane.to_string(), name.to_string()))
            .cloned()
    }

    fn worktree_root(&self, _project: &str, _repo: &str) -> Option<String> {
        self.worktree_root.clone()
    }

    fn env(&self, _project: &str, _lane: &LaneId) -> Vec<EnvEdit> {
        self.env.clone()
    }

    fn floor(&self, _project: &str, _lane: &LaneId, location: &str) -> Option<u64> {
        self.floors.get(location).copied()
    }

    fn verify_lane(
        &self,
        _project: &str,
        _lane: &LaneId,
        cx: &mut App,
    ) -> Task<Result<(), Vec<Problem>>> {
        *self.verified.lock().unwrap() += 1;
        let problems = self.verify_problems.clone();
        cx.background_spawn(async move {
            match problems {
                Some(problems) => Err(problems),
                None => Ok(()),
            }
        })
    }
}

pub(crate) fn inputs(base: &Path) -> LocationInputs {
    LocationInputs {
        project_id: "project-1".to_string(),
        project_folder: base.join("project"),
        project_repository: Some(base.join("project/repository")),
        home: base.join("home"),
        central: base.join("data/worktrees"),
        repositories: vec![RepositoryPlace {
            name: "app".to_string(),
            path: base.join("src/app"),
            template: None,
        }],
    }
}

pub(crate) fn table(text: &str) -> LaneTable {
    let table = parse(text).unwrap();
    assert!(table.diagnostics.is_empty(), "{:?}", table.diagnostics);
    table
}

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn resolve(
    inputs: &LocationInputs,
    table: &LaneTable,
    lane: &LaneId,
    profile: Option<&dyn LaneProfileSource>,
) -> LaneLocations {
    resolve_lane(
        inputs,
        &table.locations,
        binding_of(table, lane),
        lane,
        profile,
    )
    .unwrap()
}

fn path_of(locations: &LaneLocations, name: &str) -> PathBuf {
    locations.get(name).unwrap().path.clone()
}

#[test]
fn with_no_profile_and_no_locations_the_defaults_apply() {
    let base = Path::new("/w");
    let table = table(BUILD_TABLE);
    let locations = resolve(&inputs(base), &table, &build(1), None);
    let names: Vec<&str> = locations
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "tree:app",
            "tree:app:base",
            "target",
            "scratch",
            "target-base"
        ]
    );
    let tree = locations.get("tree:app").unwrap();
    assert_eq!(
        tree.path,
        PathBuf::from("/w/data/worktrees/app/lane-build-1")
    );
    assert_eq!(tree.root, PathBuf::from("/w/data"));
    assert_eq!(tree.layer, Layer::Settings(Source::Central));
    assert_eq!(
        path_of(&locations, "tree:app:base"),
        PathBuf::from("/w/data/worktrees/app/lane-build-1-base")
    );
    let target = locations.get("target").unwrap();
    assert_eq!(
        target.path,
        PathBuf::from("/w/project/lanes/build-1/target")
    );
    assert_eq!(target.root, PathBuf::from("/w/project"));
    assert_eq!(target.layer, Layer::ProjectFolder);
    assert_eq!(
        path_of(&locations, "target-base"),
        PathBuf::from("/w/project/lanes/build-1/target-base")
    );
}

#[test]
fn no_directory_is_derived_from_a_name_the_project_did_not_list() {
    let base = Path::new("/w");
    let table = table(
        "schema = 1\n[[kind]]\nname = \"build\"\n[kind.binding]\ndirs = []\ntrees = [{ repo = \"app\", base_tree = { dirs = [] } }]\n",
    );
    let locations = resolve(&inputs(base), &table, &build(1), None);
    // A fresh tree has no lane tree; its second tree gets no directory.
    let names: Vec<&str> = locations
        .items
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(names, ["tree:app:base"]);
    assert!(locations.dir_values(&[]).is_empty());

    let unbound = table_without_binding();
    assert!(
        resolve(&inputs(base), &unbound, &build(1), None)
            .items
            .is_empty()
    );
}

fn table_without_binding() -> LaneTable {
    table("schema = 1\n[[kind]]\nname = \"build\"\n")
}

#[test]
fn each_layer_wins_in_order() {
    let base = Path::new("/w");
    let mut inputs = inputs(base);
    let lane = build(1);

    // 1. The user's own worktree root.
    inputs.repositories[0].template =
        Some(("/settings/{repo}/{slug}".to_string(), TemplateLayer::Own));
    let plain = table(BUILD_TABLE);
    let tree = resolve(&inputs, &plain, &lane, None)
        .get("tree:app")
        .cloned()
        .unwrap();
    assert_eq!(tree.path, PathBuf::from("/settings/app/lane-build-1"));
    assert_eq!(tree.root, PathBuf::from("/settings"));
    assert_eq!(tree.layer, Layer::Settings(Source::Own));
    let fallback = tree.fallback.unwrap();
    assert_eq!(
        fallback.path,
        PathBuf::from("/w/data/worktrees/app/lane-build-1")
    );

    // 2. The project's [locations] wins over the settings.
    let with_locations = table(&format!(
        "{BUILD_TABLE}\n[locations]\nworktree_root = \"/project-trees/{{repo}}\"\ndirs_root = \"/project-dirs\"\n"
    ));
    let locations = resolve(&inputs, &with_locations, &lane, None);
    let tree = locations.get("tree:app").unwrap();
    assert_eq!(tree.path, PathBuf::from("/project-trees/app/lane-build-1"));
    assert_eq!(tree.root, PathBuf::from("/project-trees"));
    assert_eq!(tree.layer, Layer::Project);
    assert_eq!(tree.fallback, None);
    let target = locations.get("target").unwrap();
    assert_eq!(target.path, PathBuf::from("/project-dirs/build-1/target"));
    assert_eq!(target.root, PathBuf::from("/project-dirs"));
    assert_eq!(target.layer, Layer::Project);

    // 3. The profile's worktree root wins over the project's.
    let profile = FakeProfile {
        name: Some("fast-disk".to_string()),
        worktree_root: Some("/profile-trees".to_string()),
        ..FakeProfile::default()
    };
    let locations = resolve(&inputs, &with_locations, &lane, Some(&profile));
    let tree = locations.get("tree:app").unwrap();
    assert_eq!(tree.path, PathBuf::from("/profile-trees/lane-build-1"));
    assert_eq!(
        tree.layer,
        Layer::Profile {
            name: "fast-disk".to_string()
        }
    );
    assert_eq!(
        tree.layer.source(),
        Some(Source::Profile {
            name: "fast-disk".to_string()
        })
    );
    assert_eq!(
        path_of(&locations, "target"),
        PathBuf::from("/project-dirs/build-1/target")
    );

    // 4. The profile's own location of the tree and the directory wins over its root.
    let profile = FakeProfile {
        name: Some("fast-disk".to_string()),
        worktree_root: Some("/profile-trees".to_string()),
        ..FakeProfile::default()
    }
    .at("build-1", "tree:app", "/fast/app-1")
    .at("build-1", "target", "/fast/target-1/");
    let locations = resolve(&inputs, &with_locations, &lane, Some(&profile));
    let tree = locations.get("tree:app").unwrap();
    assert_eq!(tree.path, PathBuf::from("/fast/app-1"));
    assert_eq!(tree.root, PathBuf::from("/fast"));
    let target = locations.get("target").unwrap();
    assert_eq!(target.path, PathBuf::from("/fast/target-1"));
    assert_eq!(target.root, PathBuf::from("/fast"));
    // The second tree and the other directories keep the layers below.
    assert_eq!(
        path_of(&locations, "tree:app:base"),
        PathBuf::from("/profile-trees/lane-build-1-base")
    );
    assert_eq!(
        path_of(&locations, "scratch"),
        PathBuf::from("/project-dirs/build-1/scratch")
    );

    // 5. The lane binding's own tree location wins over everything.
    let with_own = table(&format!(
        "{}\n[locations]\nworktree_root = \"/project-trees\"\n",
        BUILD_TABLE.replace(
            "mode = \"persistent\",",
            "mode = \"persistent\", location = \"/lane-trees/{repo}\","
        )
    ));
    let locations = resolve(&inputs, &with_own, &lane, Some(&profile));
    let tree = locations.get("tree:app").unwrap();
    assert_eq!(tree.path, PathBuf::from("/lane-trees/app/lane-build-1"));
    assert_eq!(tree.root, PathBuf::from("/lane-trees"));
    assert_eq!(tree.layer, Layer::Lane);
    assert_eq!(tree.layer.source(), Some(Source::Lane));
}

#[test]
fn the_profile_is_asked_for_trees_directories_and_their_base_copies() {
    let profile = FakeProfile::default();
    resolve(
        &inputs(Path::new("/w")),
        &table(BUILD_TABLE),
        &build(1),
        Some(&profile),
    );
    assert_eq!(
        profile.asked(),
        [
            "tree:app",
            "tree:app:base",
            "target",
            "scratch",
            "target-base"
        ]
    );
}

#[test]
fn a_relative_profile_location_or_one_in_a_checkout_is_a_problem() {
    let base = Path::new("/w");
    let table = table(BUILD_TABLE);
    let profile = FakeProfile::default()
        .at("build-1", "target", "relative/target")
        .at("build-1", "scratch", "/w/src/app/scratch");
    let problems = resolve_lane(
        &inputs(base),
        &table.locations,
        binding_of(&table, &build(1)),
        &build(1),
        Some(&profile),
    )
    .unwrap_err();
    let messages: Vec<&str> = problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages[0].contains("relative path relative/target"),
        "{messages:?}"
    );
    assert_eq!(
        messages[1],
        "scratch resolves to /w/src/app/scratch, inside the checkout of app"
    );
}

#[test]
fn two_lanes_on_one_directory_refuse_naming_the_pair() {
    let base = Path::new("/w");
    let table = table(BUILD_TABLE);
    let profile = FakeProfile::default()
        .at("build-1", "target", "/x/target")
        .at("build-2", "target", "/x/./scratch/../target/");
    let lanes = table_lane_ids(&table, "project-1", Some(&profile));
    assert_eq!(lanes, [build(1), build(2)]);
    let problems = check_lanes(&inputs(base), &table, &lanes, Some(&profile)).unwrap_err();
    assert_eq!(problems.len(), 1);
    assert_eq!(
        problems[0].message,
        "build-1 target and build-2 target resolve to /x/target"
    );
}

#[test]
fn nested_directories_of_two_lanes_refuse_naming_the_pair() {
    let base = Path::new("/w");
    let table = table(BUILD_TABLE);
    let profile = FakeProfile::default()
        .at("build-1", "target-base", "/x/t")
        .at("build-2", "scratch", "/x/t/inner");
    let problems =
        check_lanes(&inputs(base), &table, &[build(1), build(2)], Some(&profile)).unwrap_err();
    assert_eq!(
        problems[0].message,
        "build-1 target-base at /x/t contains build-2 scratch at /x/t/inner"
    );

    // A shared prefix that is not a whole component does not overlap.
    let profile = FakeProfile::default()
        .at("build-1", "target", "/x/t")
        .at("build-2", "target", "/x/t2");
    check_lanes(&inputs(base), &table, &[build(1), build(2)], Some(&profile)).unwrap();
}

#[test]
fn a_raised_profile_count_is_checked_with_its_new_lanes() {
    let base = Path::new("/w");
    let table = table(&format!(
        "{BUILD_TABLE}\n[locations]\nworktree_root = \"/trees/lane\"\n"
    ));
    // Two lanes get their own folders below one root.
    let lanes = table_lane_ids(&table, "project-1", None);
    check_lanes(&inputs(base), &table, &lanes, None).unwrap();

    // A third lane the profile adds lands on a directory another lane has.
    let mut profile = FakeProfile::default().at("build-3", "target", "/w/project/lanes/build-1");
    profile.counts.insert("build".to_string(), 3);
    let lanes = table_lane_ids(&table, "project-1", Some(&profile));
    assert_eq!(lanes.len(), 3);
    let problems = check_lanes(&inputs(base), &table, &lanes, Some(&profile)).unwrap_err();
    assert_eq!(
        problems[0].message,
        "build-3 target at /w/project/lanes/build-1 contains build-1 target at \
         /w/project/lanes/build-1/target"
    );
}
