//! Who wrote a change: a person, an agent, a workflow, the healer, or nobody named.
//!
//! A person author is only ever built in the process where the person acted, from a
//! [`PersonAtKeyboard`]. Serialization never yields one: a person author is written as
//! `kind = "person"` and read back as [`AuthorKind::PersonClaimed`], which is never trusted as a
//! person.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::TaskId;

/// The kind of an [`Author`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthorKind {
    /// A person at the keyboard of this process.
    Person,
    Agent,
    Workflow,
    Healer,
    /// A write nobody said the author of.
    Unattributed,
    /// A person author read back from storage or another process: a claim, never trusted as a
    /// person.
    PersonClaimed,
}

/// Who wrote a change. The kind is private so a person author can only be built where a person
/// acted; read it with [`Author::kind`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Author {
    kind: AuthorKind,
    pub name: String,
    pub run: Option<String>,
    pub step: Option<String>,
    pub chat: Option<String>,
    /// The task and number of the proposal the change applies.
    pub proposal: Option<(TaskId, u32)>,
}

impl Author {
    fn new(kind: AuthorKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            run: None,
            step: None,
            chat: None,
            proposal: None,
        }
    }

    /// A person author. Only this crate builds one, from a [`PersonAtKeyboard`].
    #[allow(dead_code)]
    pub(crate) fn person(name: impl Into<String>) -> Self {
        Self::new(AuthorKind::Person, name)
    }

    pub fn agent(name: impl Into<String>) -> Self {
        Self::new(AuthorKind::Agent, name)
    }

    pub fn workflow(name: impl Into<String>) -> Self {
        Self::new(AuthorKind::Workflow, name)
    }

    pub fn healer(name: impl Into<String>) -> Self {
        Self::new(AuthorKind::Healer, name)
    }

    /// A write nobody named the author of; `by` says what made it.
    pub fn unattributed(by: impl Into<String>) -> Self {
        Self::new(AuthorKind::Unattributed, by)
    }

    pub fn kind(&self) -> AuthorKind {
        self.kind
    }
}

/// The kind as written: a person and a claimed person are both `person`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WireKind {
    Person,
    Agent,
    Workflow,
    Healer,
    Unattributed,
}

#[derive(Serialize, Deserialize)]
struct WireAuthor {
    kind: WireKind,
    #[serde(default)]
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposal: Option<(TaskId, u32)>,
}

impl Serialize for Author {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let kind = match self.kind {
            AuthorKind::Person | AuthorKind::PersonClaimed => WireKind::Person,
            AuthorKind::Agent => WireKind::Agent,
            AuthorKind::Workflow => WireKind::Workflow,
            AuthorKind::Healer => WireKind::Healer,
            AuthorKind::Unattributed => WireKind::Unattributed,
        };
        WireAuthor {
            kind,
            name: self.name.clone(),
            run: self.run.clone(),
            step: self.step.clone(),
            chat: self.chat.clone(),
            proposal: self.proposal.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Author {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireAuthor::deserialize(deserializer)?;
        let kind = match wire.kind {
            // Never `Person`: a person read back is only a claim.
            WireKind::Person => AuthorKind::PersonClaimed,
            WireKind::Agent => AuthorKind::Agent,
            WireKind::Workflow => AuthorKind::Workflow,
            WireKind::Healer => AuthorKind::Healer,
            WireKind::Unattributed => AuthorKind::Unattributed,
        };
        Ok(Self {
            kind,
            name: wire.name,
            run: wire.run,
            step: wire.step,
            chat: wire.chat,
            proposal: wire.proposal,
        })
    }
}

/// Proof that a person acted in this process, through a UI action of a window. It cannot be
/// copied, cloned, defaulted or serialized, so it never leaves the process or outlives its use.
#[derive(Debug)]
pub struct PersonAtKeyboard {
    _private: (),
}

impl PersonAtKeyboard {
    /// Called by UI action handlers, which have a window to pass.
    #[cfg(feature = "gpui")]
    pub fn from_action(_window: &gpui::Window, _cx: &gpui::App) -> Self {
        Self { _private: () }
    }
}
