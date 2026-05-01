use std::path::{Path, PathBuf};

/// Returns true if the string looks like a URL (scheme-based).
pub fn looks_like_url(s: &str) -> bool {
    let s = s.trim_start();
    // Scheme chars are ASCII alphanumeric + `+`, `-`, `.` per RFC 3986.
    let scheme_end =
        s.find(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.');
    match scheme_end {
        Some(i) if i > 0 => {
            let rest = &s[i..];
            rest.starts_with("://") || rest.starts_with(":/")
        }
        _ => false,
    }
}

/// Expands `~` to the user's home directory.
pub fn expand_tilde(input: &str) -> PathBuf {
    match input {
        "~" => directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(input)),
        s if s.starts_with("~/") => {
            let rest = &s[2..];
            directories::UserDirs::new()
                .map(|u| u.home_dir().join(rest))
                .unwrap_or_else(|| PathBuf::from(input))
        }
        _ => PathBuf::from(input),
    }
}

/// Attempts to complete a path against the filesystem.
/// Returns (new_input, candidate_count).
pub fn complete_path(input: &str) -> Option<(String, usize)> {
    if input.is_empty() || looks_like_url(input) {
        return None;
    }

    let expanded = expand_tilde(input);
    let (parent_dir, prefix_str) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let parent = expanded.parent().unwrap_or(Path::new(".")).to_path_buf();
        let file = expanded
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parent, file)
    };

    let mut entries: Vec<_> = match std::fs::read_dir(&parent_dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix_str) {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    Some((name, is_dir))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return None,
    };

    if entries.is_empty() {
        return Some((input.to_string(), 0));
    }

    // Sort entries for deterministic output.
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let (new_tail, hits) = if entries.len() == 1 {
        let (name, is_dir) = &entries[0];
        let tail = if *is_dir {
            format!("{}/", name)
        } else {
            name.clone()
        };
        (tail, 1)
    } else {
        // Longest common prefix of all candidate names.
        let first = &entries[0].0;
        let mut lcp_len = first.len();
        for (name, _) in &entries[1..] {
            lcp_len = lcp_len.min(common_prefix_len(first, name));
        }
        (first[..lcp_len].to_string(), entries.len())
    };

    // Replace only the trailing prefix in the original `input`.
    let base = if input.ends_with('/') {
        input.to_string()
    } else {
        // Find the last slash in the original input to preserve it (e.g., "~/Doc" -> base="~/", prefix="Doc")
        if let Some(idx) = input.rfind('/') {
            input[..=idx].to_string()
        } else {
            String::new()
        }
    };

    Some((format!("{}{}", base, new_tail), hits))
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_looks_like_url() {
        assert!(looks_like_url("http://google.com"));
        assert!(looks_like_url("https://github.com/stefan"));
        assert!(looks_like_url("gdrive://123456789"));
        assert!(looks_like_url("file:///etc/passwd"));
        assert!(looks_like_url("scheme:/path"));

        assert!(!looks_like_url("/abs/path"));
        assert!(!looks_like_url("./rel/path"));
        assert!(!looks_like_url("~/home"));
        assert!(!looks_like_url("foo.zip"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn test_expand_tilde() {
        // We can't easily test actual home dir expansion in a portable way,
        // but we can test that it handles the prefix correctly.
        let expanded = expand_tilde("~/foo");
        if let Some(user) = directories::UserDirs::new() {
            assert_eq!(expanded, user.home_dir().join("foo"));
        } else {
            assert_eq!(expanded, PathBuf::from("~/foo"));
        }

        let expanded_root = expand_tilde("~");
        if let Some(user) = directories::UserDirs::new() {
            assert_eq!(expanded_root, user.home_dir().to_path_buf());
        } else {
            assert_eq!(expanded_root, PathBuf::from("~"));
        }
    }

    #[test]
    fn test_complete_single_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("foo.zip");
        std::fs::File::create(&file_path).unwrap();

        let input = format!("{}/f", dir.path().to_string_lossy());
        let (new_input, hits) = complete_path(&input).unwrap();
        assert_eq!(hits, 1);
        assert_eq!(
            new_input,
            format!("{}/foo.zip", dir.path().to_string_lossy())
        );
    }

    #[test]
    fn test_complete_single_dir() {
        let dir = tempdir().unwrap();
        let sub_dir = dir.path().join("photos");
        std::fs::create_dir(&sub_dir).unwrap();

        let input = format!("{}/ph", dir.path().to_string_lossy());
        let (new_input, hits) = complete_path(&input).unwrap();
        assert_eq!(hits, 1);
        assert_eq!(
            new_input,
            format!("{}/photos/", dir.path().to_string_lossy())
        );
    }

    #[test]
    fn test_complete_multiple_lcp() {
        let dir = tempdir().unwrap();
        std::fs::File::create(dir.path().join("foo.zip")).unwrap();
        std::fs::File::create(dir.path().join("foobar.zip")).unwrap();

        let input = format!("{}/f", dir.path().to_string_lossy());
        let (new_input, hits) = complete_path(&input).unwrap();
        assert_eq!(hits, 2);
        assert_eq!(new_input, format!("{}/foo", dir.path().to_string_lossy()));
    }

    #[test]
    fn test_complete_no_match() {
        let dir = tempdir().unwrap();
        let input = format!("{}/xyz", dir.path().to_string_lossy());
        let (new_input, hits) = complete_path(&input).unwrap();
        assert_eq!(hits, 0);
        assert_eq!(new_input, input);
    }

    #[test]
    fn test_complete_dir_listing() {
        let dir = tempdir().unwrap();
        std::fs::File::create(dir.path().join("a.zip")).unwrap();
        std::fs::File::create(dir.path().join("b.zip")).unwrap();

        let input = format!("{}/", dir.path().to_string_lossy());
        let (new_input, hits) = complete_path(&input).unwrap();
        assert_eq!(hits, 2);
        // LCP of "a.zip" and "b.zip" is empty string, so it should stay as input.
        assert_eq!(new_input, input);
    }
}
