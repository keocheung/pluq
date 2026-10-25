//! User-facing configuration for opening archives.

use archive_core::{CodecConfig, PathPolicy, ResourceLimits};

/// Product-facing configuration — what a consumer can tune without
/// touching parser or codec internals.
#[derive(Debug, Clone, Default)]
pub struct ArchiveConfig {
    /// ZIP filename encoding; default auto, explicit values override UTF-8 flags.
    pub filename_encoding: archive_core::FilenameEncoding,
    pub limits: ResourceLimits,
    pub codec: CodecConfig,
    pub path_policy: PathPolicy,
}
