//! Who may post where without asking first, the text of the question, and its hash.
//!
//! | origin         | allowed destination | other destination |
//! |----------------|---------------------|-------------------|
//! | Kyle           | send (confirmed)    | send (confirmed)  |
//! | attended run   | send                | send              |
//! | unattended run | send                | ask               |
//! | chat agent     | send                | ask               |
//!
//! Kyle's own posts are confirmed in the minibuffer before they reach [`decide`].

use sha2::{Digest as _, Sha256};

use crate::allowed::AllowedList;
use crate::types::Destination;

/// Who asks for a post.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Kyle, through a command (confirmed in the minibuffer first).
    Kyle,
    /// A workflow run Kyle is watching.
    AttendedRun,
    /// A workflow run nobody is watching.
    UnattendedRun,
    /// An agent in a chat session.
    ChatAgent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Post now.
    Send,
    /// Hold the post and ask Kyle first.
    Ask,
}

/// Whether a post from `origin` to `destination` is sent or asked about. An empty allowed list
/// allows nothing.
pub fn decide(origin: Origin, destination: &Destination, allowed: &AllowedList) -> Decision {
    match origin {
        Origin::Kyle | Origin::AttendedRun => Decision::Send,
        Origin::UnattendedRun | Origin::ChatAgent => {
            if allowed.allows(destination) {
                Decision::Send
            } else {
                Decision::Ask
            }
        }
    }
}

/// What a post question shows: where, as whom and the exact text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostSummary<'a> {
    /// The destination as Kyle reads it, like `#releases` or `a thread in #releases`.
    pub destination: &'a str,
    /// The workspace name, like `Acme`.
    pub workspace: &'a str,
    /// The user the post is made as, like `kyle`.
    pub user: &'a str,
    pub text: &'a str,
}

impl PostSummary<'_> {
    /// `Post to #releases in Acme as kyle (412 characters):` then the exact text on the next line.
    pub fn text(&self) -> String {
        format!(
            "Post to {} in {} as {} ({} characters):\n{}",
            self.destination,
            self.workspace,
            self.user,
            self.text.chars().count(),
            self.text
        )
    }
}

/// The SHA-256 (lowercase hex) of a destination and text, each length-prefixed, so an answer to
/// one question never applies to a changed text or destination.
pub fn question_hash(destination: &Destination, text: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [destination.to_string().as_str(), text] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
