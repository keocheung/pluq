/// How extraction handles entries whose paths would escape the output directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathPolicy {
    /// Strip `..` segments and absolute roots, extract under sanitized name,
    /// surface a warning via `ProgressReporter::on_warning`. Default.
    #[default]
    Sanitize,
    /// Reject the entire extraction batch with `CoreError::PathTraversal` on
    /// the first offending entry.
    Strict,
}

/// Result of evaluating an entry's path against the configured policy.
#[derive(Debug, PartialEq, Eq)]
pub enum PathDecision {
    /// Use the entry's name unchanged (passed all checks).
    Allow,
    /// Use this rewritten relative name instead. Carries the original for warnings.
    Rewrite { sanitized: String, original: String },
    /// Skip / abort: the path is unsafe.
    Reject { path: String },
}

/// Evaluate `entry.name` against the policy. Pure, no I/O, no allocs unless rewriting.
pub fn evaluate_path(name: &str, policy: PathPolicy) -> PathDecision {
    if name.contains('\0') {
        return PathDecision::Reject {
            path: name.to_string(),
        };
    }

    let mut normalized = name;

    if normalized.starts_with(r"\\?\") || normalized.starts_with(r"\\.\") {
        normalized = &normalized[4..];
    } else if normalized.starts_with("//") {
        let mut trim_len = 2;
        while normalized[trim_len..].starts_with('/') {
            trim_len += 1;
        }
        normalized = &normalized[trim_len..];
    }

    if normalized.len() >= 2 {
        let chars = normalized.as_bytes();
        if chars[0].is_ascii_alphabetic() && chars[1] == b':' {
            normalized = &normalized[2..];
        }
    }

    while normalized.starts_with('/') || normalized.starts_with('\\') {
        normalized = &normalized[1..];
    }

    let mut segments = Vec::new();
    let mut rewritten = false;

    for segment in normalized.split(['/', '\\']) {
        match segment {
            "" | "." => {
                rewritten = true;
            }
            ".." => {
                rewritten = true;
                segments.pop();
            }
            _ => {
                segments.push(segment);
            }
        }
    }

    if normalized != name {
        rewritten = true;
    }

    let sanitized = segments.join("/");

    if sanitized.is_empty() {
        return PathDecision::Reject {
            path: name.to_string(),
        };
    }

    if rewritten || sanitized != name {
        if policy == PathPolicy::Strict {
            PathDecision::Reject {
                path: name.to_string(),
            }
        } else {
            PathDecision::Rewrite {
                sanitized,
                original: name.to_string(),
            }
        }
    } else {
        PathDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dot_dot_in_middle() {
        assert_eq!(
            evaluate_path("foo/../bar", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "bar".to_string(),
                original: "foo/../bar".to_string()
            }
        );
        assert_eq!(
            evaluate_path("foo/../bar", PathPolicy::Strict),
            PathDecision::Reject {
                path: "foo/../bar".to_string()
            }
        );
    }

    #[test]
    fn test_leading_slash() {
        assert_eq!(
            evaluate_path("/etc/passwd", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "etc/passwd".to_string(),
                original: "/etc/passwd".to_string()
            }
        );
        assert_eq!(
            evaluate_path("/etc/passwd", PathPolicy::Strict),
            PathDecision::Reject {
                path: "/etc/passwd".to_string()
            }
        );
    }

    #[test]
    fn test_leading_backslash() {
        assert_eq!(
            evaluate_path(r"\Windows\System32\evil.dll", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "Windows/System32/evil.dll".to_string(),
                original: r"\Windows\System32\evil.dll".to_string()
            }
        );
        assert_eq!(
            evaluate_path(r"\Windows\System32\evil.dll", PathPolicy::Strict),
            PathDecision::Reject {
                path: r"\Windows\System32\evil.dll".to_string()
            }
        );
    }

    #[test]
    fn test_drive_prefix() {
        assert_eq!(
            evaluate_path(r"C:\evil.exe", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "evil.exe".to_string(),
                original: r"C:\evil.exe".to_string()
            }
        );
        assert_eq!(
            evaluate_path(r"C:\evil.exe", PathPolicy::Strict),
            PathDecision::Reject {
                path: r"C:\evil.exe".to_string()
            }
        );
    }

    #[test]
    fn test_unc_prefix() {
        assert_eq!(
            evaluate_path(r"\\?\C:\evil", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "evil".to_string(),
                original: r"\\?\C:\evil".to_string()
            }
        );
        assert_eq!(
            evaluate_path(r"\\?\C:\evil", PathPolicy::Strict),
            PathDecision::Reject {
                path: r"\\?\C:\evil".to_string()
            }
        );
    }

    #[test]
    fn test_pure_dot_dot() {
        assert_eq!(
            evaluate_path("../../../etc/passwd", PathPolicy::Sanitize),
            PathDecision::Rewrite {
                sanitized: "etc/passwd".to_string(),
                original: "../../../etc/passwd".to_string()
            }
        );
        assert_eq!(
            evaluate_path("../../../etc/passwd", PathPolicy::Strict),
            PathDecision::Reject {
                path: "../../../etc/passwd".to_string()
            }
        );
    }

    #[test]
    fn test_empty_after_sanitize() {
        assert_eq!(
            evaluate_path("..", PathPolicy::Sanitize),
            PathDecision::Reject {
                path: "..".to_string()
            }
        );
        assert_eq!(
            evaluate_path("..", PathPolicy::Strict),
            PathDecision::Reject {
                path: "..".to_string()
            }
        );
    }

    #[test]
    fn test_nul_byte() {
        assert_eq!(
            evaluate_path("foo\0bar", PathPolicy::Sanitize),
            PathDecision::Reject {
                path: "foo\0bar".to_string()
            }
        );
        assert_eq!(
            evaluate_path("foo\0bar", PathPolicy::Strict),
            PathDecision::Reject {
                path: "foo\0bar".to_string()
            }
        );
    }

    #[test]
    fn test_allow_path() {
        assert_eq!(
            evaluate_path("photos/cat.jpg", PathPolicy::Sanitize),
            PathDecision::Allow
        );
        assert_eq!(
            evaluate_path("photos/cat.jpg", PathPolicy::Strict),
            PathDecision::Allow
        );
    }
}
