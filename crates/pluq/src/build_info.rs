//! Compile-time build facts, populated by `build.rs`.

pub(crate) struct BuildInfo {
    pub version: &'static str,
    pub git_sha: &'static str,
    pub target: &'static str,
    pub timestamp: i64,
}

pub(crate) fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_sha: env!("PLUQ_BUILD_GIT_SHA"),
        target: env!("PLUQ_BUILD_TARGET"),
        timestamp: env!("PLUQ_BUILD_TIMESTAMP").parse().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_populated() {
        let b = build_info();
        assert!(!b.version.is_empty());
        assert!(!b.git_sha.is_empty()); // either a real SHA or "unknown"
        assert!(!b.target.is_empty());
    }
}
