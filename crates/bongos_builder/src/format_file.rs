//! `~/.bongos/task-builder/format.txt`: the format reference, so a chat that is not planning gets
//! a pointer to it instead of the whole text. Bongos owns the file, writes it at init and never
//! reads it back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use fs::Fs;
use gpui::{App, AppContext as _};

/// The name of the format reference file.
pub const FORMAT_FILE_NAME: &str = "format.txt";

/// Where the task builder keeps its files. The app passes `<home>/.bongos/task-builder`; tests
/// pass a temporary directory, so no test touches the real home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuilderDirs {
    pub root: PathBuf,
}

impl BuilderDirs {
    /// The directory under `home`: `<home>/.bongos/task-builder`.
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".bongos").join("task-builder"),
        }
    }

    /// The path of the format reference.
    pub fn format_path(&self) -> PathBuf {
        self.root.join(FORMAT_FILE_NAME)
    }
}

/// Writes `contents` to `path` unless the file already holds exactly that. Returns whether it
/// wrote.
pub(crate) async fn write_if_different(fs: &dyn Fs, path: &Path, contents: &str) -> Result<bool> {
    if fs.load(path).await.is_ok_and(|current| current == contents) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    fs.atomic_write(path.to_path_buf(), contents.to_string())
        .await?;
    Ok(true)
}

/// Writes the format reference in the background, once per start: the file is rewritten only when
/// its content differs from `bongos_builder_model::help::FORMAT`.
pub(crate) fn write_at_init(fs: Arc<dyn Fs>, dirs: BuilderDirs, cx: &mut App) {
    cx.background_spawn(async move {
        let path = dirs.format_path();
        if let Err(error) =
            write_if_different(fs.as_ref(), &path, bongos_builder_model::help::FORMAT).await
        {
            log::warn!("task builder: could not write {}: {error}", path.display());
        }
    })
    .detach();
}
