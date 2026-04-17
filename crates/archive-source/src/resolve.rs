use archive_core::{CachingReadAt, HttpReadAt, LocalReadAt};
use archive_dropbox::{self, DropboxReadAt};
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
        return Ok(ResolvedSource::GDrive(reader, input.to_string(), file_id));
    }

    // ── Dropbox? ─────────────────────────────────────────────────────────
    if archive_dropbox::is_dropbox_url(input) {
        info!(url = %input, "Dropbox URL detected");

        let token = archive_dropbox::token().ok_or_else(|| SourceError::NoCredentials {
            provider: "Dropbox",
            guidance: "Set ARCHIVE_SNIPER_DROPBOX_TOKEN environment variable.".into(),
        })?;

        let reader = CachingReadAt::new(DropboxReadAt::new(input.to_string(), token));
        return Ok(ResolvedSource::Dropbox(reader, input.to_string()));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Use a global mutex to serialize tests that mutate the environment.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn dropbox_url_without_token_returns_no_credentials() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_TOKEN");
        }
        let result = resolve(
            "https://www.dropbox.com/s/abc/x.zip?dl=0",
            &SourceConfig::default(),
        );
        assert!(matches!(
            result,
            Err(SourceError::NoCredentials {
                provider: "Dropbox",
                ..
            })
        ));
    }

    #[test]
    fn dropbox_url_with_token_returns_dropbox_variant() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_TOKEN", "test-token");
        }
        let url = "https://www.dropbox.com/s/abc/x.zip?dl=0";
        let result = resolve(url, &SourceConfig::default());

        match result {
            Ok(ResolvedSource::Dropbox(_, r_url)) => assert_eq!(r_url, url),
            _ => panic!("expected Dropbox variant"),
        }

        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_TOKEN");
        }
    }
}
