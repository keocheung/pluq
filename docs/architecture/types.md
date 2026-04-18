# Key Types

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

Exact field shapes for the load-bearing public types.

---

## `ArchiveEntry`

Format-agnostic. Zero format-specific fields. Format-specific location
info (offsets, folder indexes, CRC values) stays inside the format
parser's internal parallel array, keyed by `index`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    // Universal fields (always meaningful)
    pub name: String,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub is_directory: bool,
    pub is_encrypted: bool,
    pub modified_at: Option<SystemTime>,

    // Display-only (optional, for UI/listing)
    pub compression_method: Option<CompressionMethod>,

    // Internal index (not format-specific, just an ID)
    pub(crate) index: usize,
}
```

> Current code may still carry private ZIP-specific fields (`offset`,
> `compression_method` as `u16`). Target shape above. Migration tracked
> separately.

---

## `ResourceLimits` & `RatioLimit`

See `cross-cutting.md` § Resource Limits for full discussion. Type
defined there for context. Reproduced here for quick lookup:

```rust
pub struct ResourceLimits {
    pub max_entries: usize,
    pub max_header_bytes: u64,
    pub max_total_unpacked: u64,
    pub max_entry_unpacked: u64,
    pub ratio_limit: Option<RatioLimit>,
}

pub struct RatioLimit {
    pub max_ratio: u32,
    pub mode: LimitMode,
}

pub enum LimitMode {
    HardError,
    Warn,
    Ignore,
}
```

---

## `ProgressReporter`

```rust
pub trait ProgressReporter: Send {
    fn on_entry_start(&mut self, name: &str, size: u64) {}
    fn on_entry_complete(&mut self, name: &str, success: bool) {}
    fn should_cancel(&self) -> bool { false }
}
```

---

## `ArchiveIndex` (cache record)

```rust
pub struct ArchiveIndex {
    pub source: String,
    pub source_size: u64,
    pub entries: Vec<ArchiveEntry>,
    pub created_at: u64,
    // Staleness detection
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
```

---

## `CodecConfig`

```rust
pub struct CodecConfig {
    /// Cap LZMA2 dictionary size. None = no cap (desktop). Some(64MB) for mobile/cloud.
    pub max_lzma_dict_size: Option<u64>,
}
```
