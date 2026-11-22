//! `lanes: accept graph`: the stamp of master's task graph and lane table, landed as
//! `definitions/lanes/stamp.toml` by the person who asked. In the writer process the action makes
//! a working copy as that person, writes the file and lands it; the person's own change is
//! approved by landing it. Once it lands, the dispatcher reads the stamp at the new verified
//! commit, the stamp check passes and the lanes pick again. With no `stamp.toml` the check is off.

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_lanes_model::{LiveFacts, StampFile, from_work_tree, parse, stamp};
use bongos_project_repo::{
    CommitId, HostBoundary, LandMode, LandRequest, LandingResult, PeerIdentity, PersonClaim,
    ProjectRepo, RepoCommand, TreeRef, clean_name, fresh_name,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_project_repo_model::{Author, ChangeMeta};
use bongos_work_model::commands::Context as WorkContext;
use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, Task};

use crate::definitions::{LANES_FILE, LANES_FOLDER};

/// The recorded stamp, inside [`LANES_FOLDER`].
pub const STAMP_FILE: &str = "stamp.toml";

/// The subject of the change that lands a stamp.
pub const ACCEPT_SUBJECT: &str = "accept the task graph";

/// What `lanes: accept graph` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// The stamp landed in `commit`.
    Landed { commit: String, digest: String },
    /// Master already records this stamp; nothing was made.
    Unchanged { digest: String },
    /// The change is made and its landing runs on in the writer. It lands as the person's own
    /// when the request came from a window that may speak for one, and otherwise waits for a
    /// person's approval (`project: approve`).
    Landing { change_id: String, digest: String },
}

/// The path of `stamp.toml` in the repository.
pub fn stamp_path() -> String {
    format!("{DEFINITIONS_DIR}/{LANES_FOLDER}/{STAMP_FILE}")
}

/// The text of the file at `path` in `commit`, `None` when it has none.
pub(crate) async fn file_at(
    repo: &ProjectRepo,
    commit: &str,
    path: &str,
) -> Result<Option<String>> {
    let repository = &repo.location().repository;
    let listed = repo
        .git()
        .run(repository, &["ls-tree", "--name-only", commit, "--", path])
        .await?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    let text = repo
        .git()
        .run(repository, &["show", &format!("{commit}:{path}")])
        .await?;
    Ok(Some(text))
}

/// The stamp recorded at `commit`; `None` when it has no `stamp.toml`, so the check is off.
pub async fn read_stamp(repo: &ProjectRepo, commit: &str) -> Result<Option<StampFile>> {
    let Some(text) = file_at(repo, commit, &stamp_path()).await? else {
        return Ok(None);
    };
    Ok(Some(StampFile::parse(&text)?))
}

/// The stamp of the task graph and the lane table at `commit`.
pub async fn stamp_at(repo: &ProjectRepo, commit: &str) -> Result<StampFile> {
    let tree = repo
        .load_work_tree_at(&TreeRef::Commit(CommitId(commit.to_string())))
        .await?;
    let project = tree
        .projects
        .first()
        .context("the project has no top group")?;
    let graph = from_work_tree(&tree, &project.slug, &LiveFacts::default())
        .context("the project's task graph cannot be read")?;
    let table_path = format!("{DEFINITIONS_DIR}/{LANES_FOLDER}/{LANES_FILE}");
    let table_text = file_at(repo, commit, &table_path)
        .await?
        .unwrap_or_else(|| "schema = 1\n".to_string());
    let table = parse(&table_text)
        .map_err(|error| anyhow!("{table_path}: line {}: {}", error.line, error.message))?;
    Ok(StampFile::from_stamp(&stamp::compute(&graph, &table)))
}

/// Lands the stamp of master as the person whose context `ctx` is, with the landing checks the
/// app registered. Runs in the writer process: elsewhere making the working copy fails.
pub fn accept_graph(
    repo: ProjectRepo,
    ctx: WorkContext,
    branch_prefix: String,
    cx: &App,
) -> Task<Result<Accepted>> {
    repo.use_registered_checks(cx);
    cx.background_spawn(
        async move { accept_graph_now(&repo, ctx, &branch_prefix, Utc::now()).await },
    )
}

/// [`accept_graph`] without an app, at `now`.
pub async fn accept_graph_now(
    repo: &ProjectRepo,
    ctx: WorkContext,
    branch_prefix: &str,
    now: DateTime<Utc>,
) -> Result<Accepted> {
    let author = ctx.author.clone();
    let (change_id, digest) = match stage_stamp(repo, author.clone(), branch_prefix, now).await? {
        Ok(staged) => staged,
        Err(done) => return Ok(done),
    };
    let request = LandRequest {
        meta: ChangeMeta::new(ACCEPT_SUBJECT, author),
        mode: LandMode::Land,
    };
    match repo.land_as_person(&change_id, request, ctx, now).await? {
        LandingResult::Landed { commit } => Ok(Accepted::Landed { commit, digest }),
        other => bail!("the stamp did not land: {other:?}"),
    }
}

/// Makes the working copy that holds the stamp of master as `author`. Resolves with its change id
/// and the stamp's digest, or with `Unchanged` when master already records the stamp.
async fn stage_stamp(
    repo: &ProjectRepo,
    author: Author,
    branch_prefix: &str,
    now: DateTime<Utc>,
) -> Result<std::result::Result<(String, String), Accepted>> {
    let master = repo.shown_commit().await?.0;
    let file = stamp_at(repo, &master).await?;
    let digest = file.digest.clone();
    if read_stamp(repo, &master).await?.as_ref() == Some(&file) {
        return Ok(Err(Accepted::Unchanged { digest }));
    }
    let copy = repo
        .create_working_copy(&clean_name(&fresh_name())?, branch_prefix, &author, now)
        .await?;
    let path = copy
        .path
        .join(DEFINITIONS_DIR)
        .join(LANES_FOLDER)
        .join(STAMP_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, file.render()?).with_context(|| format!("writing {}", path.display()))?;
    Ok(Ok((copy.change_id, digest)))
}

/// `lanes: accept graph` as the writer runs it for `peer`: the stamp lands as a person's own
/// change only when `peer` may attest a person (a window of this executable) and sent a claim;
/// from any other peer the change waits for a person's approval. Resolves once the change is
/// made ([`Accepted::Landing`]).
pub async fn accept_graph_for_peer(
    repo: &ProjectRepo,
    claim: Option<PersonClaim>,
    peer: &PeerIdentity,
    branch_prefix: &str,
    now: DateTime<Utc>,
) -> Result<Accepted> {
    let author = match (&claim, peer.may_attest_person()) {
        // A person read back is only a claim (`AuthorKind::PersonClaimed`); the grant, given
        // below for a peer that may attest one, is what makes the landing the person's own.
        (Some(claim), true) => {
            serde_json::from_value(serde_json::json!({ "kind": "person", "name": claim.name }))?
        }
        (Some(claim), false) => Author::unattributed(claim.name.clone()),
        (None, _) => Author::unattributed("lanes"),
    };
    let (change_id, digest) = match stage_stamp(repo, author.clone(), branch_prefix, now).await? {
        Ok(staged) => staged,
        Err(done) => return Ok(done),
    };
    let request = LandRequest {
        meta: ChangeMeta::new(ACCEPT_SUBJECT, author),
        mode: LandMode::Land,
    };
    // The landing runs on: it runs the landing checks, and with no person's grant it waits for
    // one's approval, so the request does not wait for it (a client gives a writer ten seconds
    // to answer). The caller hears of the end through the repository's landing events.
    let boundary = HostBoundary::new(repo.clone());
    let peer = peer.clone();
    let landing_id = change_id.clone();
    smol::spawn(async move {
        let command = RepoCommand::Land {
            change_id: landing_id.clone(),
            request,
            person: claim,
        };
        if let Err(error) = boundary.accept_now(command, &peer, Utc::now()).await {
            log::error!("landing the accepted task graph {landing_id}: {error:#}");
        }
    })
    .detach();
    Ok(Accepted::Landing { change_id, digest })
}
