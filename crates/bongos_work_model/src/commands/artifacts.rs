//! The artifact commands (D22): `artifact: hide`, `share`, `hide all` and `share all`, which write
//! `artifacts.toml` only; `artifact: link`, which writes a `<name>.link.toml`; and `task: set
//! default visibility`, which writes `task.toml`.
//!
//! The artifact commands record no history, because they leave `task.toml` alone: the entry of
//! their [`Change`] names the event and is written nowhere.

use super::{Change, Context, Outcome, Write, edit_task, writable};
use crate::LinkFile;
use crate::artifacts::task_artifacts;
use crate::layout::LINK_SUFFIX;
use crate::rules::Place;
use crate::slug::{slugify, unique_slug};
use crate::store::WorkTree;
use crate::types::{ArtifactsFile, Visibility};

pub fn visibility_name(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Shared => "shared",
        Visibility::HumanOnly => "human-only",
    }
}

/// A change that replaces only the `artifacts.toml` of the task at `place`.
fn manifest_change(
    ctx: &Context,
    place: &Place,
    file: ArtifactsFile,
    event: &str,
    detail: Option<String>,
) -> Change {
    let dir = place.task.dir.clone();
    Change {
        dir: dir.clone(),
        writes: vec![Write::Artifacts { dir, file }],
        entry: ctx.entry(event, detail),
        moves: Vec::new(),
    }
}

fn event(visibility: Visibility, all: bool) -> &'static str {
    match (visibility, all) {
        (Visibility::HumanOnly, false) => "artifact hidden",
        (Visibility::Shared, false) => "artifact shared",
        (Visibility::HumanOnly, true) => "artifacts hidden",
        (Visibility::Shared, true) => "artifacts shared",
    }
}

fn set_visibility(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    path: &str,
    visibility: Visibility,
) -> Outcome {
    let (place, _) = writable(tree, id)?;
    let mut list = task_artifacts(place.task);
    let artifact = list
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == path)
        .ok_or_else(|| format!("{} has no artifact {path}", place.slug()))?;
    if artifact.visibility == visibility {
        return Err(format!("{path} is already {}", visibility_name(visibility)));
    }
    artifact.visibility = visibility;
    Ok(manifest_change(
        ctx,
        &place,
        list,
        event(visibility, false),
        Some(path.to_string()),
    ))
}

fn set_all(tree: &WorkTree, ctx: &Context, id: &str, visibility: Visibility) -> Outcome {
    let (place, _) = writable(tree, id)?;
    let mut list = task_artifacts(place.task);
    if list.artifacts.is_empty() {
        return Err(format!("{} has no artifacts", place.slug()));
    }
    if list
        .artifacts
        .iter()
        .all(|artifact| artifact.visibility == visibility)
    {
        return Err(format!(
            "Every artifact of {} is already {}",
            place.slug(),
            visibility_name(visibility)
        ));
    }
    for artifact in &mut list.artifacts {
        artifact.visibility = visibility;
    }
    Ok(manifest_change(
        ctx,
        &place,
        list,
        event(visibility, true),
        None,
    ))
}

/// `artifact: hide`: the artifact at `path` (its manifest key, such as `notes/plan.md`) of the task
/// with `id` becomes human-only.
pub fn artifact_hide(tree: &WorkTree, ctx: &Context, id: &str, path: &str) -> Outcome {
    set_visibility(tree, ctx, id, path, Visibility::HumanOnly)
}

/// `artifact: share`: the artifact at `path` of the task with `id` becomes shared.
pub fn artifact_share(tree: &WorkTree, ctx: &Context, id: &str, path: &str) -> Outcome {
    set_visibility(tree, ctx, id, path, Visibility::Shared)
}

/// `artifact: hide all`: every artifact of the task with `id` becomes human-only.
pub fn artifact_hide_all(tree: &WorkTree, ctx: &Context, id: &str) -> Outcome {
    set_all(tree, ctx, id, Visibility::HumanOnly)
}

/// `artifact: share all`: every artifact of the task with `id` becomes shared.
pub fn artifact_share_all(tree: &WorkTree, ctx: &Context, id: &str) -> Outcome {
    set_all(tree, ctx, id, Visibility::Shared)
}

/// `artifact: link`: a link to a document that lives elsewhere (such as a Notion page), written as
/// `<slug of title>.link.toml` in the task's directory. It is an artifact like any file, with the
/// task's default visibility until it is hidden or shared.
pub fn artifact_link(tree: &WorkTree, ctx: &Context, id: &str, url: &str, title: &str) -> Outcome {
    let (place, _) = writable(tree, id)?;
    let url = url.trim();
    if url.is_empty() {
        return Err("a link needs a URL".to_string());
    }
    let title = match title.trim() {
        "" => url,
        title => title,
    };
    let taken: Vec<String> = place
        .task
        .files
        .keys()
        .filter_map(|path| path.to_str())
        .filter_map(|name| name.strip_suffix(LINK_SUFFIX))
        .map(str::to_string)
        .collect();
    let name = unique_slug(&slugify(title), &taken);
    let dir = place.task.dir.clone();
    let path = dir.join(format!("{name}{LINK_SUFFIX}"));
    Ok(Change {
        dir,
        writes: vec![Write::Link {
            path,
            file: LinkFile::new(url, title),
        }],
        entry: ctx.entry("artifact linked", Some(url.to_string())),
        moves: Vec::new(),
    })
}

/// `task: set default visibility`: the visibility a file not yet in `artifacts.toml` gets. Files
/// already in the manifest keep theirs.
pub fn set_default_visibility(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    visibility: Visibility,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        if file.default_visibility == visibility {
            return Err(format!(
                "{} already defaults to {}",
                place.slug(),
                visibility_name(visibility)
            ));
        }
        file.default_visibility = visibility;
        Ok((
            "default visibility set",
            Some(visibility_name(visibility).to_string()),
        ))
    })
}
