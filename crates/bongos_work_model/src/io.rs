//! Parsing, reading and serializing the four file kinds. Every file starts with `schema = 1`; a
//! file with another schema, or none, is rejected with an error that says so.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, ErrorKind, Result};
use crate::types::{ArtifactsFile, LinkFile, ProjectFile, SCHEMA, TaskFile};

/// A file with a `schema` key at the top.
pub trait SchemaFile: Serialize + DeserializeOwned {
    fn schema(&self) -> u32;
}

macro_rules! schema_file {
    ($($ty:ty),*) => {$(
        impl SchemaFile for $ty {
            fn schema(&self) -> u32 {
                self.schema
            }
        }
    )*};
}
schema_file!(TaskFile, ProjectFile, ArtifactsFile, LinkFile);

#[derive(serde::Deserialize)]
struct SchemaProbe {
    schema: Option<toml::Value>,
}

fn parse<T: SchemaFile>(text: &str, path: Option<&Path>) -> Result<T> {
    let probe: SchemaProbe =
        toml::from_str(text).map_err(|error| Error::syntax(text, &error).with_path(path))?;
    match probe.schema {
        Some(toml::Value::Integer(version)) if version == i64::from(SCHEMA) => {}
        found => {
            let found = found.map(|value| value.to_string());
            return Err(Error::new(ErrorKind::UnsupportedSchema { found }).with_path(path));
        }
    }
    toml::from_str(text).map_err(|error| Error::syntax(text, &error).with_path(path))
}

fn read<T: SchemaFile>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::io(path, error))?;
    parse(&text, Some(path))
}

fn serialize<T: SchemaFile>(file: &T) -> Result<String> {
    if file.schema() != SCHEMA {
        return Err(Error::new(ErrorKind::UnsupportedSchema {
            found: Some(file.schema().to_string()),
        }));
    }
    toml::to_string(file).map_err(|error| Error::new(ErrorKind::Serialize(error.to_string())))
}

macro_rules! file_functions {
    ($ty:ty, $parse:ident, $read:ident, $serialize:ident) => {
        pub fn $parse(text: &str) -> Result<$ty> {
            parse(text, None)
        }

        pub fn $read(path: &Path) -> Result<$ty> {
            read(path)
        }

        pub fn $serialize(file: &$ty) -> Result<String> {
            serialize(file)
        }
    };
}

file_functions!(TaskFile, parse_task, read_task, serialize_task);
file_functions!(ProjectFile, parse_project, read_project, serialize_project);
file_functions!(
    ArtifactsFile,
    parse_artifacts,
    read_artifacts,
    serialize_artifacts
);
file_functions!(LinkFile, parse_link, read_link, serialize_link);

pub(crate) fn parse_task_at(text: &str, path: &Path) -> Result<TaskFile> {
    parse(text, Some(path))
}

pub(crate) fn parse_project_at(text: &str, path: &Path) -> Result<ProjectFile> {
    parse(text, Some(path))
}

pub(crate) fn parse_artifacts_at(text: &str, path: &Path) -> Result<ArtifactsFile> {
    parse(text, Some(path))
}
