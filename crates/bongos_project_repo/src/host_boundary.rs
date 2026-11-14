//! Where the project's host takes commands and state writes from other processes (feature 48's
//! socket calls it once per request, with the [`PeerIdentity`] read when the connection was
//! accepted). A command's [`PersonClaim`] becomes a person's approval only when the peer may
//! attest a person: a window running this executable. Otherwise the command is applied with its
//! author as a claimed person, which approves nothing, and the change waits for `project:
//! approve` from a verified window.

use anyhow::{Result, anyhow};
use bongos_project_repo_model::Author;
use chrono::{DateTime, Utc};

use crate::approval::{NEEDS_A_WINDOW, PersonGrant};
use crate::create::ProjectRepo;
use crate::peer::PeerIdentity;
use crate::sink::{PersonClaim, RepoCommand, RepoReply};
use crate::state_ops::StateOp;

/// The host side of one project's commands from other processes.
pub struct HostBoundary {
    repo: ProjectRepo,
}

/// `author` as it is after crossing a process boundary: a person becomes a claimed person.
fn as_received(author: &Author) -> Result<Author> {
    Ok(serde_json::from_value(serde_json::to_value(author)?)?)
}

/// The person's grant for `claim`, only from a peer that may attest a person.
fn grant(claim: Option<PersonClaim>, peer: &PeerIdentity) -> Option<PersonGrant> {
    let claim = claim?;
    if !peer.may_attest_person() {
        log::info!(
            "not trusting the person claim of pid {} ({:?}, same executable: {})",
            peer.pid(),
            peer.client(),
            peer.same_executable()
        );
        return None;
    }
    Some(PersonGrant::for_verified_peer(claim.name))
}

impl HostBoundary {
    pub fn new(repo: ProjectRepo) -> Self {
        Self { repo }
    }

    /// Runs `command` from `peer`, with the checks registered in the app.
    pub fn accept(
        &self,
        command: RepoCommand,
        peer: &PeerIdentity,
        cx: &gpui::App,
    ) -> gpui::Task<Result<RepoReply>> {
        self.repo.use_registered_checks(cx);
        let boundary = Self {
            repo: self.repo.clone(),
        };
        let peer = peer.clone();
        gpui::AppContext::background_spawn(cx, async move {
            boundary.accept_now(command, &peer, Utc::now()).await
        })
    }

    /// [`Self::accept`] without an app, at `now`.
    pub async fn accept_now(
        &self,
        command: RepoCommand,
        peer: &PeerIdentity,
        now: DateTime<Utc>,
    ) -> Result<RepoReply> {
        match command {
            RepoCommand::Approve { change_id, person } => {
                let grant = grant(Some(person), peer).ok_or_else(|| {
                    anyhow!("{NEEDS_A_WINDOW}; the request did not come from a verified window")
                })?;
                self.repo.approve_with(&change_id, grant, now).await?;
                Ok(RepoReply::Done)
            }
            RepoCommand::Land {
                change_id,
                mut request,
                person,
            } => {
                request.meta.author = as_received(&request.meta.author)?;
                let grant = grant(person, peer);
                self.repo
                    .land_granted(&change_id, request, grant, now)
                    .await?;
                Ok(RepoReply::Done)
            }
        }
    }

    /// Applies state ops sent by `peer`. `origin` only labels history and is taken as received:
    /// at most a claimed person. State ops need no approval.
    pub fn apply(&self, ops: Vec<StateOp>, origin: Author, peer: &PeerIdentity) -> Result<()> {
        let origin = as_received(&origin)?;
        let writer = self.repo.writer_or_error()?;
        let notes = writer.transact_with(&origin, &ops, |tx| tx.apply_ops(&ops))?;
        for note in notes {
            log::info!("{note} (from pid {})", peer.pid());
        }
        Ok(())
    }
}
