//! Making a lane's locations before a job: the profile's verify, the overlap check, the disk
//! floors, then the trees and directories made only below existing roots, and the
//! `after_tree_created` hooks for each tree made for the first time. Any failure leaves the lane
//! waiting with the reason; a failure before the making starts leaves the disk as it was.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_lanes_model::table::{Binding, LaneTable};
use bongos_lanes_model::{LaneId, LaneRecord, LaneState, Problem, WaitReason};
use gpui::{App, AppContext as _, Task};

use crate::create::{check_root, create_below_existing};
use crate::hooks::{AFTER_TREE_CREATED, JobHooks};
use crate::locations::{
    LaneLocation, LaneLocations, Layer, LocationInputs, Place, binding_of, check_overlap,
    resolve_lane,
};
use crate::profile::LaneProfileSource;

/// Makes a lane tree: `git worktree add` in the app. It runs on a background thread.
pub type MakeTree = Arc<dyn Fn(&TreeRequest) -> anyhow::Result<()> + Send + Sync>;

/// Free bytes on the disk that holds a path, `None` when unknown (then the floor passes).
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// A lane tree to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRequest {
    pub lane: LaneId,
    pub repo: String,
    /// The repository's primary checkout.
    pub repository_path: PathBuf,
    pub path: PathBuf,
    /// Whether this is the second tree.
    pub base: bool,
}

pub struct PrepareRequest {
    pub inputs: LocationInputs,
    pub table: Arc<LaneTable>,
    pub lane: LaneId,
    /// Every lane of the project that is not stopped and within its count; the lane's
    /// locations must not overlap theirs.
    pub live: Vec<LaneId>,
    pub make_tree: MakeTree,
    pub free_space: FreeSpace,
}

/// A lane whose locations all exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLane {
    /// The locations as made (a tree of the user's own root may have gone to the central path).
    pub locations: LaneLocations,
    /// The trees made by this call, as `(repo, path)`.
    pub created_trees: Vec<(String, PathBuf)>,
}

/// The free space of the disk holding `path`, from the system.
pub fn system_free_space() -> FreeSpace {
    Arc::new(bongos_workflow::worktrees::disk_free_bytes)
}

/// `lane` waiting with `reason`.
pub fn wait_lane(lane: &LaneRecord, reason: WaitReason) -> LaneRecord {
    LaneRecord {
        state: LaneState::Waiting,
        reason: Some(reason),
        ..lane.clone()
    }
}

/// Prepares the locations of `request.lane`. With `profile` set, its `verify_lane` must resolve
/// `Ok` before anything else happens, and its floors are checked; with none, neither runs.
pub fn prepare_lane(
    request: PrepareRequest,
    profile: Option<Arc<dyn LaneProfileSource>>,
    cx: &mut App,
) -> Task<Result<PreparedLane, WaitReason>> {
    let verify = profile
        .as_ref()
        .map(|profile| profile.verify_lane(&request.inputs.project_id, &request.lane, cx));
    let hooks = JobHooks::hooks(cx);
    cx.spawn(async move |cx| {
        if let Some(verify) = verify
            && let Err(problems) = verify.await
        {
            return Err(problems_reason(&problems));
        }
        let profile = profile.as_deref();
        let locations =
            resolve_checked(&request, profile).map_err(|problems| problems_reason(&problems))?;
        if let (Some(profile), Some(binding)) = (profile, binding_of(&request.table, &request.lane))
        {
            check_floors(
                profile,
                &request.inputs.project_id,
                binding,
                &locations,
                request.free_space.as_ref(),
            )
            .map_err(WaitReason::Environment)?;
        }
        let repositories = request.inputs.repositories.clone();
        let make_tree = request.make_tree.clone();
        let prepared = cx
            .background_spawn(async move { make_locations(locations, &repositories, &make_tree) })
            .await
            .map_err(WaitReason::Environment)?;
        for (repo, path) in &prepared.created_trees {
            for hook in &hooks {
                let task = cx.update(|cx| hook.after_tree_created(&request.lane, repo, path, cx));
                task.await.map_err(|message| WaitReason::Hook {
                    point: AFTER_TREE_CREATED.to_string(),
                    message,
                })?;
            }
        }
        Ok(prepared)
    })
}

/// The problems as one wait reason.
fn problems_reason(problems: &[Problem]) -> WaitReason {
    WaitReason::Environment(
        problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// The lane's locations, after checking them against the other live lanes'. A live lane whose
/// own locations do not resolve has its own wait; it does not hold this one.
fn resolve_checked(
    request: &PrepareRequest,
    profile: Option<&dyn LaneProfileSource>,
) -> Result<LaneLocations, Vec<Problem>> {
    let table = &request.table;
    let resolve = |lane: &LaneId| {
        resolve_lane(
            &request.inputs,
            &table.locations,
            binding_of(table, lane),
            lane,
            profile,
        )
    };
    let own = resolve(&request.lane)?;
    let mut all = vec![own.clone()];
    all.extend(
        request
            .live
            .iter()
            .filter(|lane| **lane != request.lane)
            .filter_map(|lane| resolve(lane).ok()),
    );
    check_overlap(&all).map_err(|message| {
        vec![Problem {
            requirement: "every lane has its own trees and directories".to_string(),
            message,
            hint: None,
        }]
    })?;
    Ok(own)
}

/// Refuses when the disk of a location in the binding's `floors` has less free space than the
/// profile's floor for it.
pub fn check_floors(
    profile: &dyn LaneProfileSource,
    project: &str,
    binding: &Binding,
    locations: &LaneLocations,
    free_space: &dyn Fn(&Path) -> Option<u64>,
) -> Result<(), String> {
    for floor in &binding.floors {
        let name = floor.value.as_str();
        let Some(minimum) = profile.floor(project, &locations.lane, name) else {
            continue;
        };
        let Some(location) = locations.get(name) else {
            continue;
        };
        if let Some(free) = free_space(&location.path)
            && free < minimum
        {
            return Err(format!(
                "disk below floor on {name}: {} of {}",
                bytes(free),
                bytes(minimum)
            ));
        }
    }
    Ok(())
}

fn bytes(count: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if count >= GIB {
        format!("{:.1} GiB", count as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", count as f64 / MIB as f64)
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Checks every root first, so a refusal makes nothing; then makes the directories and trees.
fn make_locations(
    mut locations: LaneLocations,
    repositories: &[crate::locations::RepositoryPlace],
    make_tree: &MakeTree,
) -> Result<PreparedLane, String> {
    let lane = locations.lane.clone();
    for item in &mut locations.items {
        if exists(&item.path) {
            continue;
        }
        let Err(error) = check_root(&item.root) else {
            continue;
        };
        let refusal = |item: &LaneLocation, error: &dyn std::fmt::Display| {
            format!(
                "{lane} {}: {error} (the root {} is set by {}); nothing was created",
                item.name,
                item.root.display(),
                item.layer
            )
        };
        let falls_back = matches!(&item.layer, Layer::Settings(source) if source.falls_back());
        match item.fallback.clone() {
            Some(fallback) if falls_back => {
                if !exists(&fallback.path)
                    && let Err(fallback_error) = check_root(&fallback.root)
                {
                    return Err(format!(
                        "{}, and the central root {} cannot be used either: {fallback_error}",
                        refusal(item, &error),
                        fallback.root.display()
                    ));
                }
                item.path = fallback.path;
                item.root = fallback.root;
                item.fallback = None;
            }
            _ => return Err(refusal(item, &error)),
        }
    }

    let mut created_trees = Vec::new();
    for item in &locations.items {
        let made = |error: crate::create::CreateError| format!("{lane} {}: {error}", item.name);
        match &item.place {
            Place::Dir { .. } => {
                if !exists(&item.path) {
                    create_below_existing(&item.path, &item.root).map_err(made)?;
                }
            }
            Place::Tree { repo, base } => {
                if exists(&item.path) {
                    continue;
                }
                let parent = item.path.parent().unwrap_or(&item.path);
                create_below_existing(parent, &item.root).map_err(made)?;
                let repository_path = repositories
                    .iter()
                    .find(|repository| &repository.name == repo)
                    .map(|repository| repository.path.clone())
                    .unwrap_or_default();
                make_tree(&TreeRequest {
                    lane: lane.clone(),
                    repo: repo.clone(),
                    repository_path,
                    path: item.path.clone(),
                    base: *base,
                })
                .map_err(|error| format!("{lane} {}: {error:#}", item.name))?;
                created_trees.push((repo.clone(), item.path.clone()));
            }
        }
    }
    Ok(PreparedLane {
        locations,
        created_trees,
    })
}
