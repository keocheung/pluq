use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

pub fn scrub(input: &str) -> Cow<'_, str> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();

    let url_re = URL_RE.get_or_init(|| Regex::new(r"https?://[^\s]+").unwrap());

    let token_re = TOKEN_RE.get_or_init(|| {
        Regex::new(r#"(?i)(authorization|bearer|access_token|refresh_token|token|api[_-]?key|client[_-]?secret)(["':= ]+)[^\s"',&]+"#).unwrap()
    });

    let mut result = Cow::Borrowed(input);
    if url_re.is_match(&result) {
        result = Cow::Owned(url_re.replace_all(&result, "<url>").into_owned());
    }
    if token_re.is_match(&result) {
        result = Cow::Owned(
            token_re
                .replace_all(&result, "${1}${2}<redacted>")
                .into_owned(),
        );
    }
    result
}

fn collapse_homes(input: &str, real_home: Option<&str>) -> String {
    static USER_HOME_RE: OnceLock<Regex> = OnceLock::new();

    let mut s = scrub(input).into_owned();

    if let Some(home_str) = real_home
        && home_str.len() > 1
    {
        s = s.replace(home_str, "~");
    }

    // /Users/<name>, /home/<name>, C:\Users\<name>  ->  ~
    let re = USER_HOME_RE
        .get_or_init(|| Regex::new(r"(?i)(?:/Users/|/home/|[A-Z]:\\Users\\)[^/\\]+").unwrap());
    re.replace_all(&s, "~").into_owned()
}

/// Stronger scrub for diagnostic dumps (bug reports, crash reports).
///
/// Runs the standard URL + token `scrub`, then collapses home directories to `~`:
/// the current user's real home first (most precise), then generic per-OS user-home
/// prefixes so paths belonging to *other* users are anonymised too.
pub fn scrub_report(input: &str) -> String {
    let home_dir = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf());
    let real_home = home_dir.as_deref().and_then(|p| p.to_str());
    collapse_homes(input, real_home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrub_url() {
        assert_eq!(
            scrub("opening https://example.com/file.zip now"),
            "opening <url> now"
        );
    }

    #[test]
    fn test_scrub_token() {
        assert_eq!(scrub("Bearer xyz123"), "Bearer <redacted>");
        assert_eq!(scrub("access_token=\"abc\""), "access_token=\"<redacted>\"");
    }

    #[test]
    fn test_scrub_report_unix_home() {
        let out = scrub_report("opening /home/someoneelse/data/secret.zip");
        assert!(out.contains("~/data/secret.zip"), "got: {out}");
        assert!(!out.contains("someoneelse"), "got: {out}");
    }

    #[test]
    fn test_scrub_report_macos_home() {
        let out = scrub_report("/Users/jdoe/Documents/a.rar");
        assert!(!out.contains("jdoe"), "got: {out}");
        assert!(out.contains('~'), "got: {out}");
    }

    #[test]
    fn test_scrub_report_windows_home() {
        let out = scrub_report(r"C:\Users\jdoe\stuff");
        assert!(!out.contains("jdoe"), "got: {out}");
    }

    #[test]
    fn test_scrub_report_still_redacts_urls_and_tokens() {
        let out = scrub_report("GET https://drive.google.com/x Bearer abc123");
        assert!(out.contains("<url>"), "got: {out}");
        assert!(out.contains("<redacted>"), "got: {out}");
    }

    #[test]
    fn test_collapse_homes_root_safety() {
        let out = collapse_homes("opening /var/log/foo", Some("/"));
        assert_eq!(out, "opening /var/log/foo");
    }

    #[test]
    fn test_collapse_homes_real_home() {
        let out = collapse_homes("file in /Users/jdoe/stuff", Some("/Users/jdoe"));
        assert_eq!(out, "file in ~/stuff");
    }

    #[test]
    fn test_collapse_homes_windows_space() {
        let out = collapse_homes(r"C:\Users\John Doe\x", None);
        assert_eq!(out, r"~\x");
    }
}
