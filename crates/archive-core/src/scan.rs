/// Describes whether an archive's metadata is ready or requires a scan phase.
///
/// Returned by format parsers to let the facade decide whether to present
/// the archive as immediately usable or requiring a scan step.
#[derive(Debug, Clone)]
pub enum ScanInfo {
    /// Metadata already available. `entries()` works immediately.
    Ready,
    /// Metadata scan required before entries are available.
    Required {
        /// Estimated bytes to stream through during the scan.
        estimated_bytes: u64,
        /// Human-readable explanation for the product/UI layer.
        reason: &'static str,
    },
}
