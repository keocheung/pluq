/// Load the Dropbox access token.
///
/// Priority: env var `ARCHIVE_SNIPER_DROPBOX_TOKEN` > embedded default.
/// Returns `None` if no token is available.
pub fn token() -> Option<String> {
    if let Ok(t) = std::env::var("ARCHIVE_SNIPER_DROPBOX_TOKEN")
        && !t.is_empty()
    {
        return Some(t);
    }
    option_env!("ARCHIVE_SNIPER_DROPBOX_TOKEN_EMBED").map(|s| s.to_string())
}
