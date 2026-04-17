use url::Url;

/// Check if a URL is a Dropbox shared link.
///
/// Matches both legacy (`/s/`) and modern (`/scl/`) shared link formats.
pub fn is_dropbox_url(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !matches!(host, "www.dropbox.com" | "dropbox.com") {
        return false;
    }
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    matches!(segments.first().copied(), Some("s") | Some("scl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_legacy_shared_link() {
        assert!(is_dropbox_url(
            "https://www.dropbox.com/s/abc123/file.zip?dl=0"
        ));
    }

    #[test]
    fn detects_scl_shared_link() {
        assert!(is_dropbox_url(
            "https://www.dropbox.com/scl/fi/xyz/file.zip?rlkey=foo"
        ));
    }

    #[test]
    fn detects_bare_dropbox_host() {
        assert!(is_dropbox_url("https://dropbox.com/s/abc/file.zip"));
    }

    #[test]
    fn rejects_non_dropbox_host() {
        assert!(!is_dropbox_url("https://example.com/s/abc/file.zip"));
    }

    #[test]
    fn rejects_dropbox_non_share_path() {
        assert!(!is_dropbox_url("https://www.dropbox.com/home"));
    }

    #[test]
    fn rejects_garbage_url() {
        assert!(!is_dropbox_url("not-a-url"));
    }
}
