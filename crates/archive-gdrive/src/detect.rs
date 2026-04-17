use url::Url;

/// Check if a URL is a Google Drive share link.
pub fn is_gdrive_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h == "drive.google.com"))
        .unwrap_or(false)
}

/// Extract the file ID from a Google Drive URL.
///
/// Supports:
/// - `https://drive.google.com/file/d/{FILE_ID}/view?usp=sharing`
/// - `https://drive.google.com/uc?id={FILE_ID}&export=download`
/// - `https://drive.google.com/open?id={FILE_ID}`
///
/// Returns `None` if no file ID can be found.
pub fn extract_file_id(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;

    // Path-based: /file/d/{FILE_ID}/...
    let segments: Vec<&str> = parsed.path_segments()?.collect();
    if let Some(pos) = segments.iter().position(|&s| s == "d")
        && let Some(&id) = segments.get(pos + 1)
        && !id.is_empty()
    {
        return Some(id.to_string());
    }

    // Query-based: ?id={FILE_ID}
    parsed
        .query_pairs()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_gdrive_url() {
        assert!(is_gdrive_url("https://drive.google.com/file/d/ABC/view"));
        assert!(!is_gdrive_url("https://example.com/file.zip"));
        assert!(!is_gdrive_url("not-a-url"));
    }

    #[test]
    fn test_extract_file_id() {
        assert_eq!(
            extract_file_id("https://drive.google.com/file/d/1A2B3C_xyz/view?usp=sharing"),
            Some("1A2B3C_xyz".to_string())
        );
        assert_eq!(
            extract_file_id("https://drive.google.com/uc?id=1A2B3C_xyz&export=download"),
            Some("1A2B3C_xyz".to_string())
        );
        assert_eq!(
            extract_file_id("https://drive.google.com/open?id=1A2B3C_xyz"),
            Some("1A2B3C_xyz".to_string())
        );
        assert_eq!(
            extract_file_id("https://drive.google.com/drive/my-drive"),
            None
        );
        assert_eq!(extract_file_id("https://example.com/file.zip"), None);
    }
}
