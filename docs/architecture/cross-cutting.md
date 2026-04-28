# Cross-Cutting Concerns

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

Concerns that span multiple layers: integrity, progress, multi-volume
discovery, resource limits, timestamps.

---

## CRC / Integrity Verification

**Rule: always verify after decompression, never silently return
potentially corrupt bytes.**

- ZIP: CRC32 stored per-entry in Central Directory → verify after Deflate
  decompression.
- 7z: CRC32 per-substream in SubStreamsInfo + per-folder in UnpackInfo.
- RAR: CRC32 per-file.
- TAR: weak header checksum only (no per-file CRC).

Return a CRC mismatch error with entry name, expected, and actual values.
CRC mismatch is also the primary wrong-password detection mechanism for
encrypted archives. See `docs/errors/archive-error.md` for how CRC errors
flow through layers.

---

## Progress Reporting

**Status:** implemented

```rust
pub trait ProgressReporter: Send {
    fn on_entry_start(&mut self, name: &str, size: u64) {}
    fn on_entry_complete(&mut self, name: &str, success: bool) {}
    fn on_bytes_progress(&mut self, bytes_so_far: u64) {}
    fn on_scan_start(&mut self, total_bytes: u64) {}
    fn on_scan_progress(&mut self, bytes_scanned: u64, entries_found: usize) {}
    fn on_scan_complete(&mut self, entries_found: usize) {}

    /// A non-fatal safety warning was raised. Typical source:
    /// `ResourceLimits` check in `LimitMode::Warn` (e.g. "suspicious
    /// 800:1 ratio"). The operation continues.
    fn on_warning(&mut self, message: &str) {}

    fn should_cancel(&self) -> bool { false }
}
```

- Independent of streaming — works with standard extraction.
- `should_cancel()` replaces a separate cancellation channel.
- Returns a cancellation error when true.
- Wrap with `ThrottledProgress` (minimum interval between callbacks) for
  UI.

---

## Multi-Volume Archives

Handled at the I/O layer via `MultiVolumeReadAt`:

- User passes first volume filename.
- I/O layer discovers siblings by format-specific naming convention.
- Builds prefix-sum size table for offset translation.
- Format parser sees one seamless ReadAt.

Volume naming conventions:

- 7z: `archive.7z.001`, `.002`, `.003`, …
- ZIP: `archive.z01`, `.z02`, …, `archive.zip`
- RAR: `archive.part1.rar`, `.part2.rar`, …

---

## Resource Limits (anti-bomb)

**Status:** implemented

Apply at parse time (header reading) and extraction time. Checked before
any decompression begins.

```rust
pub struct ResourceLimits {
    pub max_entries: usize,              // default: 1,000,000 — entry count bomb
    pub max_header_bytes: u64,           // default: 64 MiB   — oversized header DoS
    pub max_total_unpacked: u64,         // default: 1 TiB    — total extraction size
    pub max_entry_unpacked: u64,         // default: 64 GiB   — single entry size
    pub ratio_limit: Option<RatioLimit>, // default: 1000:1   — zip bomb ratio check
}

pub struct RatioLimit {
    pub max_ratio: u32,   // uncompressed / compressed threshold
    pub mode: LimitMode,
}

pub enum LimitMode {
    HardError,  // return Err (default — safe)
    Warn,       // surface warning to UI, continue
    Ignore,     // skip check (for advanced users)
}
```

**Ratio check implementation:** use
`uncompressed > max_ratio * compressed` (multiplication, not division —
avoids integer truncation errors on fractional ratios). Check for
infinite ratio separately: `compressed == 0 && uncompressed > 0`.

**`LimitMode::Warn`** is useful for the extraction tax UI: show "this
archive has suspicious compression ratio (800:1)" without blocking. Mode
is per-limit so you can warn on ratio but hard-error on entry count.

**Streaming for solid blocks** *[deferred]*: a 50 GB solid block
shouldn't be buffered entirely in memory. Design the `ArchiveReader`
trait to support streaming extraction mode, implement when 7z support is
built.

---

## Path Traversal Protection

**Status:** implemented

Two modes (no third "Relaxed" mode — there is no legitimate use case for
writing outside the output directory):

- **Sanitize** (default): strip `../` and absolute path components,
  extract with sanitized name, warn user. Maximally compatible.
- **Strict**: reject archive entirely if any entry contains path
  traversal. For high-security contexts.

Symlink target verification ensures links stay within the output
directory.

---

## Timestamps

Each format stores timestamps differently:

- ZIP: 32-bit DOS packed field (2-second precision, years since 1980).
- 7z: Windows FILETIME (100-nanosecond precision, epoch 1601-01-01).
- TAR: Unix timestamp (seconds since 1970).
- RAR: Windows FILETIME (RAR5) or 32-bit DOS FTIME (RAR4).

`ArchiveEntry` stores: `pub modified_at: Option<SystemTime>`.

Conversion constant for 7z FILETIME → Unix: subtract `116444736000000000`,
divide by `10_000_000`.
