use archive::Source;
use archive_gdrive::{self, GDriveError, GoogleDriveReadAt};
use tracing::info;

/// Detect the provider from a URL and build the appropriate reader.
///
/// Returns `Some(Source)` if a cloud provider was detected and the reader
/// was built successfully. Returns `None` for non-provider URLs (caller
/// should use `Archive::open()` with the raw string instead).
///
/// Returns `Err` if a provider was detected but setup failed (e.g. missing API key).
pub fn build_reader(url: &str) -> Result<Option<Source>, String> {
    if !archive_gdrive::is_gdrive_url(url) {
        return Ok(None);
    }

    info!(url = %url, "Google Drive URL detected");

    let api_key = archive_gdrive::api_key()
        .ok_or("Google Drive URL detected but no API key configured. Set ARCHIVE_SNIPER_GDRIVE_KEY environment variable.")?;

    let file_id = archive_gdrive::extract_file_id(url)
        .ok_or_else(|| format!("Could not extract file ID from Google Drive URL: {url}"))?;

    let reader = GoogleDriveReadAt::new(file_id, api_key);
    Ok(Some(Source::provider(reader)))
}

/// Format a provider Transport error into a user-friendly message.
///
/// Called by the CLI when catching ArchiveError::Transport.
/// Returns `None` if the error is not from a known provider.
pub fn format_transport_error(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    err.downcast_ref::<GDriveError>().map(|e| e.to_string())
}
