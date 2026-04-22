//! User-facing configuration for opening archives.

use archive_core::{CodecConfig, ResourceLimits};

/// Product-facing configuration — what a consumer can tune without
/// touching parser or codec internals.
#[derive(Debug, Clone, Default)]
pub struct ArchiveConfig {
    pub limits: ResourceLimits,
    pub codec: CodecConfig,
}
