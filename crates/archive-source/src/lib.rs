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
/// Strips directories, query strings, and one file extension. For URL sources
/// the basename is percent-decoded so an escaped name reads naturally.
/// Special handling for Google Drive "view", "uc", and "open" links to extract the File ID.
///
/// - `photos.rar` -> `Some("photos")`
/// - `https://example.com/data.zip?q=1` -> `Some("data")`
/// - `https://example.com/Bard%27s%20Tale.rar` -> `Some("Bard's Tale")`
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
    // Percent-decode for URL sources so e.g. "Bard%27s%20Tale.rar" becomes
    // "Bard's Tale.rar". Local paths are taken verbatim — a real file may
    // legitimately contain a '%'.
    let decoded: std::borrow::Cow<'_, str> = if is_url_source(source) {
        percent_encoding::percent_decode_str(filename).decode_utf8_lossy()
    } else {
        std::borrow::Cow::Borrowed(filename)
    };
    let stem = Path::new(decoded.as_ref()).file_stem()?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string_lossy().to_string())
}

/// Whether a source string is an HTTP(S) URL (vs. a local filesystem path).
fn is_url_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

#[cfg(test)]
mod suggest_name_tests {
    use super::suggest_name_from_str;

    #[test]
    fn plain_paths_and_urls() {
        assert_eq!(
            suggest_name_from_str("photos.rar").as_deref(),
            Some("photos")
        );
        assert_eq!(
            suggest_name_from_str("/tmp/backup.zip").as_deref(),
            Some("backup")
        );
        assert_eq!(
            suggest_name_from_str("https://example.com/data.zip?q=1").as_deref(),
            Some("data")
        );
    }

    #[test]
    fn url_basename_is_percent_decoded() {
        assert_eq!(
            suggest_name_from_str("https://example.com/Bard%27s%20Tale%20IV.rar").as_deref(),
            Some("Bard's Tale IV")
        );
    }

    #[test]
    fn local_path_with_percent_is_verbatim() {
        // A real local file may legitimately contain '%' — do not decode it.
        assert_eq!(
            suggest_name_from_str("/tmp/100%25_real.zip").as_deref(),
            Some("100%25_real")
        );
    }
}
