use archive_core::{CachingReadAt, HttpReadAt, LocalReadAt};
use archive_gdrive::{self, GoogleDriveReadAt};
use tracing::info;

use crate::config::SourceConfig;
use crate::error::SourceError;
use crate::resolved::ResolvedSource;

/// Resolve user input into a fully-constructed reader.
///
/// Detects the source type (local path, cloud provider URL, or generic HTTP),
/// looks up credentials, and constructs the appropriate `ReadAt` implementation.
///
/// Returns `Err(SourceError)` if a provider was detected but setup failed
/// (e.g., missing API key, unparseable URL).
pub fn resolve(input: &str, _config: &SourceConfig) -> Result<ResolvedSource, SourceError> {
    // ── Google Drive? ────────────────────────────────────────────────────
    if archive_gdrive::is_gdrive_url(input) {
        info!(url = %input, "Google Drive URL detected");

        let api_key = archive_gdrive::api_key().ok_or_else(|| SourceError::NoCredentials {
            provider: "Google Drive",
            guidance: "Set ARCHIVE_SNIPER_GDRIVE_KEY environment variable.".into(),
        })?;

        let file_id =
            archive_gdrive::extract_file_id(input).ok_or_else(|| SourceError::InvalidUrl {
                url: input.to_string(),
                reason: "could not extract Google Drive file ID".into(),
            })?;

        let reader = CachingReadAt::new(GoogleDriveReadAt::new(file_id.clone(), api_key));
        return Ok(ResolvedSource::GDrive(
            reader,
            input.to_string(),
            file_id,
        ));
    }

    // ── Phase 4: Dropbox check would go here ─────────────────────────────

    // ── Generic HTTP? ────────────────────────────────────────────────────
    if input.starts_with("http://") || input.starts_with("https://") {
        info!(url = %input, "generic HTTP URL detected");
        let client = reqwest::Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        let reader = CachingReadAt::new(HttpReadAt::new(input, client));
        return Ok(ResolvedSource::Http(reader, input.to_string()));
    }

    // ── Local file ───────────────────────────────────────────────────────
    info!(path = %input, "local file path detected");
    Ok(ResolvedSource::Local(
        LocalReadAt::new(input),
        input.to_string(),
    ))
}
