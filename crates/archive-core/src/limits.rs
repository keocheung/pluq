//! Resource limits and codec configuration — anti-bomb and memory safety.
//!
//! Applied at parse time (entry count, header size) and extraction time
//! (compression ratio, per-entry size, total unpacked). See
//! `docs/architecture/cross-cutting.md` § Resource Limits for the full
//! design rationale.

/// Anti-bomb and anti-DoS limits applied at parse and extract time.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum number of entries in the archive. Default: 1,000,000.
    pub max_entries: usize,
    /// Maximum bytes of header data read during parse. Default: 64 MiB.
    pub max_header_bytes: u64,
    /// Maximum total uncompressed bytes across a batch extraction.
    /// Default: 1 TiB.
    pub max_total_unpacked: u64,
    /// Maximum uncompressed bytes for a single entry. Default: 64 GiB.
    pub max_entry_unpacked: u64,
    /// Compression-ratio guard. `None` disables the check.
    /// Default: `Some(RatioLimit { max_ratio: 1000, mode: HardError })`.
    pub ratio_limit: Option<RatioLimit>,
}

/// Compression ratio policy: `uncompressed > max_ratio * compressed` →
/// violation.
#[derive(Debug, Clone, Copy)]
pub struct RatioLimit {
    pub max_ratio: u32,
    pub mode: LimitMode,
}

/// Enforcement mode for an individual limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitMode {
    /// Return `CoreError::ResourceLimitExceeded`. Safe default.
    HardError,
    /// Surface a warning via `ProgressReporter::on_warning`, continue.
    Warn,
    /// Skip the check entirely.
    Ignore,
}

/// Codec-layer memory limits (currently only LZMA/LZMA2).
#[derive(Debug, Clone, Default)]
pub struct CodecConfig {
    /// Cap LZMA/LZMA2 dictionary size in bytes.
    /// `None` = no cap (desktop default).
    /// `Some(128 * 1024 * 1024)` recommended for mobile/server.
    pub max_lzma_dict_size: Option<u64>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_header_bytes: 64 * 1024 * 1024, // 64 MiB
            max_total_unpacked: 1024 * 1024 * 1024 * 1024, // 1 TiB
            max_entry_unpacked: 64 * 1024 * 1024 * 1024, // 64 GiB
            ratio_limit: Some(RatioLimit {
                max_ratio: 1000,
                mode: LimitMode::HardError,
            }),
        }
    }
}

impl RatioLimit {
    /// Returns `true` if the (compressed, uncompressed) pair violates this
    /// limit.
    ///
    /// Rule:
    /// - `compressed == 0 && uncompressed > 0` → always a violation
    ///   (infinite ratio).
    /// - otherwise: `uncompressed > max_ratio as u64 * compressed`.
    ///
    /// Uses multiplication rather than division to avoid integer
    /// truncation errors on fractional ratios. Uses `u128` for the
    /// multiplication to prevent overflow on pathological inputs.
    pub fn is_violation(&self, compressed: u64, uncompressed: u64) -> bool {
        if compressed == 0 {
            return uncompressed > 0;
        }
        let threshold = (self.max_ratio as u128) * (compressed as u128);
        (uncompressed as u128) > threshold
    }
}

impl ResourceLimits {
    /// Helper to enforce `max_entry_unpacked` and `ratio_limit` during extraction.
    ///
    /// If `is_dynamic` is true, uses "suspicious ratio" messaging (vague/runtime);
    /// if false, uses "exceeds limit" messaging (exact/pre-check).
    ///
    /// If `skip_warn` is true, the `on_warning` callback is suppressed for
    /// `LimitMode::Warn` violations (to avoid double-warning in loops), but
    /// the check still returns `Ok(true)`.
    ///
    /// Returns `Ok(warned)` on success or `LimitMode::Warn`.
    /// Returns `Err(CoreError::ResourceLimitExceeded)` on `LimitMode::HardError`.
    pub fn check_extraction_limits(
        &self,
        entry_name: &str,
        compressed_size: u64,
        uncompressed_size: u64,
        progress: &std::sync::Arc<std::sync::Mutex<dyn crate::progress::ProgressReporter>>,
        is_dynamic: bool,
        skip_warn: bool,
    ) -> Result<bool, crate::CoreError> {
        if uncompressed_size > self.max_entry_unpacked {
            return Err(crate::CoreError::ResourceLimitExceeded {
                message: if is_dynamic {
                    format!(
                        "max_entry_unpacked ({} bytes) exceeded for '{}'",
                        self.max_entry_unpacked, entry_name
                    )
                } else {
                    format!(
                        "entry '{}' declared size {} exceeds max_entry_unpacked {}",
                        entry_name, uncompressed_size, self.max_entry_unpacked
                    )
                },
            });
        }

        if let Some(ratio) = &self.ratio_limit
            && ratio.is_violation(compressed_size, uncompressed_size)
        {
            let message = if is_dynamic {
                format!(
                    "suspicious compression ratio for '{}': {} bytes from {} compressed",
                    entry_name, uncompressed_size, compressed_size
                )
            } else {
                format!(
                    "entry '{}' compression ratio exceeds {}:1 (compressed={}, uncompressed={})",
                    entry_name, ratio.max_ratio, compressed_size, uncompressed_size
                )
            };

            match ratio.mode {
                LimitMode::HardError => {
                    return Err(crate::CoreError::ResourceLimitExceeded { message });
                }
                LimitMode::Warn => {
                    if !skip_warn {
                        progress.lock().unwrap().on_warning(&message);
                    }
                    return Ok(true);
                }
                LimitMode::Ignore => {}
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_limits_defaults_match_spec() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_entries, 1_000_000);
        assert_eq!(limits.max_header_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_total_unpacked, 1024 * 1024 * 1024 * 1024);
        assert_eq!(limits.max_entry_unpacked, 64 * 1024 * 1024 * 1024);

        let ratio = limits.ratio_limit.expect("should have ratio limit");
        assert_eq!(ratio.max_ratio, 1000);
        assert_eq!(ratio.mode, LimitMode::HardError);
    }

    #[test]
    fn ratio_helper_uses_multiplication_not_division() {
        let limit = RatioLimit {
            max_ratio: 1000,
            mode: LimitMode::HardError,
        };
        // True ratio is ~1000.33. If division (3001/3 = 1000) was used, it wouldn't flag as > 1000.
        // With multiplication, 3001 > 3 * 1000 -> 3001 > 3000 -> true.
        assert!(limit.is_violation(3, 3001));
        // And for <= exactly it should be false
        assert!(!limit.is_violation(3, 3000));
    }

    #[test]
    fn ratio_helper_handles_zero_compressed() {
        let limit = RatioLimit {
            max_ratio: 1000,
            mode: LimitMode::HardError,
        };
        assert!(limit.is_violation(0, 1));
        assert!(!limit.is_violation(0, 0));
    }

    #[test]
    fn ratio_helper_handles_large_inputs_without_overflow() {
        let limit = RatioLimit {
            max_ratio: 10,
            mode: LimitMode::HardError,
        };
        let compressed = u64::MAX / 2;
        // The multiplication inside is_violation would overflow u64.
        // It should evaluate correctly to false because 0 is not > 10 * (u64::MAX / 2).
        assert!(!limit.is_violation(compressed, 0));
        assert!(!limit.is_violation(compressed, u64::MAX)); // u64::MAX is < 10 * (u64::MAX / 2), so false, no panic.
    }

    #[test]
    fn codec_config_default_is_uncapped() {
        let config = CodecConfig::default();
        assert_eq!(config.max_lzma_dict_size, None);
    }
}
