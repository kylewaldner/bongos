//! The allowed list: the conversations runs and the chat agent may post to without asking.

use std::collections::BTreeSet;

use crate::types::{ConversationId, Destination};

/// A set of conversations. The default list is empty and allows nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedList {
    conversations: BTreeSet<ConversationId>,
}

impl AllowedList {
    pub fn new(conversations: impl IntoIterator<Item = ConversationId>) -> Self {
        Self {
            conversations: conversations.into_iter().collect(),
        }
    }

    pub fn contains(&self, conversation: &ConversationId) -> bool {
        self.conversations.contains(conversation)
    }

    /// Whether posts to `destination` are allowed: its conversation is listed, whatever the
    /// thread.
    pub fn allows(&self, destination: &Destination) -> bool {
        self.contains(&destination.conversation)
    }

    /// Adds `conversation`; false when it was already listed.
    pub fn insert(&mut self, conversation: ConversationId) -> bool {
        self.conversations.insert(conversation)
    }

    /// Removes `conversation`; false when it was not listed.
    pub fn remove(&mut self, conversation: &ConversationId) -> bool {
        self.conversations.remove(conversation)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ConversationId> {
        self.conversations.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.conversations.is_empty()
    }
}
