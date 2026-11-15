//! Check 7, `size-and-kind`: text files stay small, images stay in a `documents/` folder and
//! under their own limit, and no other binary file lands.

use super::outcome::CheckOutcome;
use crate::layout::DOCUMENTS_DIR;

/// The extensions read as images.
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
/// How many leading bytes are looked at for a NUL.
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// The limits of check 7 (the settings `project_repository.text_file_limit_bytes` and
/// `image_limit_bytes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeLimits {
    pub text_file_limit_bytes: u64,
    pub image_limit_bytes: u64,
}

impl Default for SizeLimits {
    fn default() -> Self {
        Self {
            text_file_limit_bytes: 1024 * 1024,
            image_limit_bytes: 5 * 1024 * 1024,
        }
    }
}

/// One changed file: its path, its size, and its first bytes (at least the first
/// [`BINARY_SNIFF_BYTES`] when it is that long).
#[derive(Clone, Copy, Debug)]
pub struct FileFacts<'a> {
    pub path: &'a str,
    pub len: u64,
    pub head: &'a [u8],
}

/// Check 7 over the changed files.
pub fn size_and_kind<'a>(
    files: impl IntoIterator<Item = FileFacts<'a>>,
    limits: SizeLimits,
) -> CheckOutcome {
    let mut refusals = Vec::new();
    for file in files {
        let path = file.path;
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase());
        let image = extension
            .as_deref()
            .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension));
        if image {
            let in_documents = path
                .rsplit_once('/')
                .is_some_and(|(folder, _)| folder.split('/').any(|part| part == DOCUMENTS_DIR));
            if !in_documents {
                refusals.push(format!(
                    "{path}: an image belongs in a {DOCUMENTS_DIR}/ folder"
                ));
            }
            if file.len > limits.image_limit_bytes {
                refusals.push(format!(
                    "{path}: an image of {} bytes is over the limit of {}",
                    file.len, limits.image_limit_bytes
                ));
            }
            continue;
        }
        let sniffed = &file.head[..file.head.len().min(BINARY_SNIFF_BYTES)];
        if sniffed.contains(&0) {
            refusals.push(format!("{path}: a binary file never lands"));
        } else if file.len > limits.text_file_limit_bytes {
            refusals.push(format!(
                "{path}: a text file of {} bytes is over the limit of {}",
                file.len, limits.text_file_limit_bytes
            ));
        }
    }
    CheckOutcome::from_refusals(refusals)
}
