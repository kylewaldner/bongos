//! Reading `allowed.toml`, the conversations each project's runs and chat agents may post to
//! without asking: `[projects."<project id>"] conversations = ["C0123", ...]`. This module only
//! reads it; a missing or unreadable file reads as empty.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use bongos_slack_model::{AllowedList, ConversationId};
use serde::Deserialize;

use crate::connection::{allowed_path, default_dir};

#[derive(Default, Deserialize)]
struct AllowedFile {
    #[serde(default)]
    projects: BTreeMap<String, ProjectEntry>,
}

#[derive(Default, Deserialize)]
struct ProjectEntry {
    #[serde(default)]
    conversations: Vec<String>,
}

/// The allowed list of `project` in the application's Slack directory.
pub fn load(project: &str) -> AllowedList {
    load_in(&default_dir(), project)
}

/// The allowed list of `project` in `allowed.toml` of `dir`; empty when the file is missing or
/// unreadable.
pub fn load_in(dir: &Path, project: &str) -> AllowedList {
    let path = allowed_path(dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return AllowedList::default(),
        Err(error) => {
            log::warn!("could not read the Slack allowed list: {error}");
            return AllowedList::default();
        }
    };
    let mut file: AllowedFile = match toml::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            log::warn!("the Slack allowed list is not valid: {error}");
            return AllowedList::default();
        }
    };
    let conversations = file
        .projects
        .remove(project)
        .map(|entry| entry.conversations)
        .unwrap_or_default();
    AllowedList::new(conversations.into_iter().map(ConversationId))
}
