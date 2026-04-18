/// Load the Dropbox app credentials (key + secret) for App Authentication.
///
/// Priority: env vars `ARCHIVE_SNIPER_DROPBOX_APP_KEY` + `ARCHIVE_SNIPER_DROPBOX_APP_SECRET`,
/// falling back to embedded defaults (`*_EMBED` compile-time vars).
/// Returns `None` if either component is missing.
pub fn app_credentials() -> Option<(String, String)> {
    let key = std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("ARCHIVE_SNIPER_DROPBOX_APP_KEY_EMBED").map(String::from))?;
    let secret = std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("ARCHIVE_SNIPER_DROPBOX_APP_SECRET_EMBED").map(String::from))?;
    Some((key, secret))
}
