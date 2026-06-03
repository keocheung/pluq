/// Detection and matching for glob patterns.
///
/// Wraps the `glob` crate with gitignore-style semantics:
/// - Patterns containing `/` match against the full archive path.
/// - Patterns without `/` match against the entry's basename only.
///
/// Returns true if the string contains glob meta-characters.
pub fn is_glob_query(q: &str) -> bool {
    q.bytes().any(|b| matches!(b, b'*' | b'?' | b'['))
}

/// Compile a glob pattern, returning None if the pattern is invalid.
pub fn compile_glob(q: &str) -> Option<glob::Pattern> {
    glob::Pattern::new(q).ok()
}

/// Match an entry name against a glob pattern using gitignore-style semantics.
///
/// A pattern containing `/` matches against the full entry path
/// (e.g. `photos/**/*.jpg`). A pattern with no `/` matches only against the
/// entry's basename (e.g. `*.jpg` matches any jpg anywhere in the tree).
pub fn glob_matches(pattern: &glob::Pattern, entry_name: &str) -> bool {
    let options = glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let pat_str = pattern.as_str();
    if pat_str.contains('/') {
        if pat_str.starts_with('/') {
            if entry_name.starts_with('/') {
                pattern.matches_with(entry_name, options)
            } else {
                let prefixed = format!("/{}", entry_name);
                pattern.matches_with(&prefixed, options)
            }
        } else {
            pattern.matches_with(entry_name, options)
        }
    } else {
        let basename = entry_name.rsplit('/').next().unwrap_or(entry_name);
        pattern.matches_with(basename, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_glob_query() {
        assert!(is_glob_query("*.jpg"));
        assert!(is_glob_query("?ake.txt"));
        assert!(is_glob_query("[abc].md"));
        assert!(is_glob_query("photos/*/*.png"));

        assert!(!is_glob_query("cat"));
        assert!(!is_glob_query("dog.jpg"));
        assert!(!is_glob_query(""));
        assert!(!is_glob_query("photos/cat.jpg"));
    }

    #[test]
    fn test_glob_matches_basename() {
        let p = glob::Pattern::new("*.jpg").unwrap();
        assert!(glob_matches(&p, "photo.jpg"));
        assert!(glob_matches(&p, "photos/vacation/img.jpg"));
        assert!(!glob_matches(&p, "photos/vacation/img.png"));
    }

    #[test]
    fn test_glob_matches_fullpath() {
        let p = glob::Pattern::new("photos/**/*.jpg").unwrap();
        assert!(glob_matches(&p, "photos/vacation/img.jpg"));
        assert!(glob_matches(&p, "photos/a/b/c.jpg"));
        assert!(!glob_matches(&p, "docs/img.jpg"));
        // basename-only pattern shouldn't match here
        assert!(!glob_matches(&p, "photos.jpg"));

        // Test root-level matching with **
        let p2 = glob::Pattern::new("**/*.jpg").unwrap();
        assert!(glob_matches(&p2, "img.jpg"));

        // Test leading slash matching
        let p3 = glob::Pattern::new("/*.jpg").unwrap();
        assert!(glob_matches(&p3, "img.jpg"));
        assert!(!glob_matches(&p3, "photos/img.jpg"));

        // Test case-insensitivity
        let p4 = glob::Pattern::new("*.jpg").unwrap();
        assert!(glob_matches(&p4, "IMG.JPG"));
    }
}
