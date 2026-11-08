use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Replaces `path` with `bytes` so a crash leaves the old file or the new one, never a mix.
///
/// Writes `.<name>.tmp-<pid>-<random>` in the same directory, syncs it, renames it over the
/// target, then syncs the directory. If anything before the rename fails, the target is left
/// untouched and the temporary file is removed. The parent directory must exist.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |file| file.write_all(bytes))
}

/// [`write_atomic`] with the write step supplied by the caller. This is the seam tests use to
/// inject a write failure.
pub(crate) fn write_atomic_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    // Dropping the temporary file on an early return removes it.
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{name}.tmp-{}-", std::process::id()))
        .rand_bytes(8)
        .tempfile_in(directory)?;
    write(temp.as_file_mut())?;
    temp.as_file().sync_all()?;
    if let Ok(metadata) = std::fs::metadata(path) {
        // The temporary file is private by default; keep the mode the target already had.
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.persist(path).map_err(|error| error.error)?;
    File::open(directory)?.sync_all()
}
