mod classify;
mod config;
mod error;
mod resolve;
mod resolved;

pub use classify::{ProviderError, classify_transport};
pub use config::SourceConfig;
pub use error::SourceError;
pub use resolve::resolve;
pub use resolved::ResolvedSource;

use std::path::Path;

/// Suggest a filename stem from a source string (URL or path).
///
/// Strips directories, query strings, and one file extension.
/// Special handling for Google Drive "view", "uc", and "open" links to extract the File ID.
///
/// - `photos.rar` -> `Some("photos")`
/// - `https://example.com/data.zip?q=1` -> `Some("data")`
/// - `https://drive.google.com/file/d/XYZ/view` -> `Some("XYZ")`
pub fn suggest_name_from_str(source: &str) -> Option<String> {
    // ── Google Drive specific logic ──────────────────────────────────────
    if archive_gdrive::is_gdrive_url(source)
        && let Some(id) = archive_gdrive::extract_file_id(source)
    {
        let filename = source
            .split('?')
            .next()?
            .rsplit('/')
            .next()
            .or_else(|| source.rsplit('\\').next())?;
        if filename == "view" || filename == "uc" || filename == "open" {
            return Some(id);
        }
    }

    // ── Generic logic ────────────────────────────────────────────────────
    let filename = source
        .rsplit('/')
        .next()
        .or_else(|| source.rsplit('\\').next())?;
    let filename = filename.split('?').next()?;
    let stem = Path::new(filename).file_stem()?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string_lossy().to_string())
}
