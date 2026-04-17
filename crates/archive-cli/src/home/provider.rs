use archive_source::ResolvedSource;

/// Display tag for a source provider. Mirrors `ResolvedSource` variants plus
/// forward-compat `Dropbox` (Phase 4 per SOURCE.md) so `recent.json` schema
/// is stable across the Phase 4 cut-over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Provider {
    Local,
    Http,
    GDrive,
    Dropbox,
}

impl Provider {
    /// Derive from the authoritative `ResolvedSource`. Zero string parsing —
    /// `archive-source::resolve()` is the single source of truth for provider
    /// detection. Every `ResolvedSource` variant maps to exactly one tag.
    pub fn from_resolved(src: &ResolvedSource) -> Self {
        match src {
            ResolvedSource::Local(..) => Self::Local,
            ResolvedSource::Http(..) => Self::Http,
            ResolvedSource::GDrive(..) => Self::GDrive,
            // When ResolvedSource::Dropbox lands (Phase 4), add arm here.
        }
    }

    /// Short tag shown in Home Screen lists — e.g. ` [GDrive]`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Local => "[Local]",
            Self::Http => "[Http]",
            Self::GDrive => "[GDrive]",
            Self::Dropbox => "[Dropbox]",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tags() {
        assert_eq!(Provider::Local.tag(), "[Local]");
        assert_eq!(Provider::Http.tag(), "[Http]");
        assert_eq!(Provider::GDrive.tag(), "[GDrive]");
        assert_eq!(Provider::Dropbox.tag(), "[Dropbox]");
    }

    #[test]
    fn test_serde_round_trip() {
        let providers = vec![
            Provider::Local,
            Provider::Http,
            Provider::GDrive,
            Provider::Dropbox,
        ];

        for p in providers {
            let json = serde_json::to_string(&p).unwrap();
            let de: Provider = serde_json::from_str(&json).unwrap();
            assert_eq!(p, de);
        }
    }

    #[test]
    fn test_serde_names() {
        assert_eq!(serde_json::to_string(&Provider::Local).unwrap(), "\"Local\"");
        assert_eq!(serde_json::to_string(&Provider::Http).unwrap(), "\"Http\"");
        assert_eq!(serde_json::to_string(&Provider::GDrive).unwrap(), "\"GDrive\"");
        assert_eq!(serde_json::to_string(&Provider::Dropbox).unwrap(), "\"Dropbox\"");
    }
}
