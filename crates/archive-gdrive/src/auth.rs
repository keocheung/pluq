/// Load the Google Drive API key.
///
/// Priority: env var `ARCHIVE_SNIPER_GDRIVE_KEY` > embedded default.
/// Returns `None` if no key is available.
pub fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ARCHIVE_SNIPER_GDRIVE_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    option_env!("ARCHIVE_SNIPER_GDRIVE_KEY_EMBED").map(|s| s.to_string())
}
