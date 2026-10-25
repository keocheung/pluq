# Layer 2 — Format Parsers

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

Per-format parsers (zip, rar, rar4, future: 7z, tar). Each implements
`ArchiveReader`. Each holds entries (universal) and locations (private,
format-specific). Each translates its method IDs to the shared
`CompressionMethod` enum.

---

## `ArchiveReader` Trait

```rust
pub trait ArchiveReader {
    fn entries(&self) -> &[ArchiveEntry];
    fn entries_clean(&self) -> Vec<&ArchiveEntry>;

    /// Bytes that must be downloaded+decompressed to produce this entry.
    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64;

    /// Extract entry, streaming decompressed bytes to `sink`. Returns bytes written.
    /// The base interface is always a sink — never returns Bytes directly.
    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self, entry: &ArchiveEntry, sink: &mut W,
    ) -> Result<u64, CoreError>;

    /// Decompress + verify CRC, write nothing. Essential for integrity checking.
    async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError>;
}
```

**Sink-based extraction.** `extract()` writes decompressed bytes to any
`AsyncWrite` — a file, an HTTP response body, a `Vec<u8>`, `/dev/null`
(for test mode). The caller decides the destination, core never buffers
the full output.

**Convenience for in-memory use** (CLI, small entries, tests) — available
on `Archive`:

```rust
impl Archive {  // state layer Archive
    pub async fn extract_to_bytes(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError> {
        let mut buf = Vec::with_capacity(entry.uncompressed_size as usize);
        self.extract(entry, &mut buf).await?;
        Ok(Bytes::from(buf))
    }
}
```

`extraction_cost()` enables the "extraction tax" UX:

- **ZIP**: returns `entry.compressed_size` (every entry is independently
  seekable).
- **7z solid block**: returns
  `sum(compressed_sizes_of_all_preceding_entries_in_folder) + entry.compressed_size`.
- State layer / UI shows: *"Sniping this 1 MB file requires downloading
  500 MB. Proceed?"*

---

## Internal Location Data

Each format parser keeps location details PRIVATE — never on
`ArchiveEntry`:

```rust
// ZIP: offset into the archive
struct ZipEntryLocation { offset: u64, method_id: u16, crc32: u32 }

// 7z: position within a solid block
struct SevenZEntryLocation { folder_index: usize, stream_index: usize, crc32: u32 }

// TAR: byte offset + stream-level compression
struct TarEntryLocation { data_offset: u64 }
```

Format parser holds `entries: Vec<ArchiveEntry>` + `locations: Vec<FormatLocation>`
as parallel arrays. Same index maps entry → location.

---

## Format-Specific Method ID Translation

Each format translates its internal IDs to the shared `CompressionMethod`
enum:

```rust
// ZIP
fn zip_method(id: u16) -> Result<CompressionMethod> {
    match id {
        0 => Ok(CompressionMethod::Store),
        8 => Ok(CompressionMethod::Deflate),
        12 => Ok(CompressionMethod::BZip2),
        14 => Ok(CompressionMethod::Lzma),
        93 => Ok(CompressionMethod::Zstd),
        _ => Err(CoreError::UnsupportedCompression { algorithm: id.to_string() }),
    }
}

// 7z
fn sevenz_method(id: &[u8]) -> Result<CompressionMethod> {
    match id {
        [0x00] => Ok(CompressionMethod::Store),
        [0x21] => Ok(CompressionMethod::Lzma2),
        [0x04, 0x01, 0x08] => Ok(CompressionMethod::Deflate),
        _ => Err(CoreError::UnsupportedCompression { algorithm: id.to_string() }),
    }
}
```

---

## The `has_stream` Flag

Entries in 7z (and RAR) can be "empty stream" — directories and anti-files
that occupy zero compressed space. Don't use `uncompressed_size == 0` as a
proxy for "no data" — some real files are 0 bytes. The `has_stream: bool`
concept must be explicit in the format parser's internal data (not on
`ArchiveEntry`, but in `FormatLocation`). Entries without streams are
skipped entirely during the folder→entry mapping scan.

---

## Solid Block Handling (7z, RAR)

SubStreams = the slicing information that divides a decompressed solid
block into individual files. A Folder (solid block) decompresses to one
blob. SubStreamsInfo says how many files are in it and their sizes.

**Extraction from solid block:**

1. Decompress the entire folder.
2. Skip `sum(sizes[0..stream_index])` bytes (preceding files).
3. Read `sizes[stream_index]` bytes (target file).

**Sniping policy:** Solid archives are supported. When a user wants to
pluck from a solid block, compute the cost (how many bytes must be
decompressed and discarded to reach their file), inform them in
user-friendly language, then proceed if they accept. Not anti-solid —
just transparent about the cost.

---

## 7z Header Parsing — Cloud Sniping Implications

### Encoded Headers (recursive)

A 7z Start Header points to a "Next Header" block. That block is either:

- **Plain** (`0x01`): parse directly → done.
- **Encoded** (`0x17`): the header itself is compressed (and optionally
  encrypted). Must decompress it first to get the real header.

**The recursion:** encoded headers can themselves contain encoded headers.
The spec requires:

- MUST support at least 1 level of encoding.
- SHOULD support up to 4 levels (real archives exist with 2 levels).
- MUST reject archives with more than 4 levels (DoS protection against
  infinite recursion).

Parsing algorithm: loop until plain header is found, count levels, error
on >4.

**Cloud sniping cost:** Each encoding level requires an additional HTTP
round trip to fetch and decompress the header data before a file listing
can be shown. A 2-level encoded archive needs 3 requests before the user
sees any filenames.

### Encrypted Headers

When the encoded header's coder chain includes AES, **filenames and
metadata are hidden** until the password is provided. This is not an
error — it's a feature. The correct UX response:

> "This archive has encrypted metadata. Enter password to view contents."

Password-required is not an error from the user's perspective — the state
layer handles it by prompting for a password and retrying. See
`docs/errors/archive-error.md` for the full error translation flow.

### BCJ2 Folders — Multiple Pack Streams

BCJ2 is structurally different from all other filters. It has **4 input
streams** (Main, Call, Jump, Range) each stored as separate pack streams
at different offsets in the archive. The `Folder` struct's `uses_bcj2()`
flag signals this.

When extracting from a BCJ2 folder:

1. Check `folder.uses_bcj2()` — if true, use the BCJ2-specific decoder
   path.
2. Issue up to 4 separate `read_stream()` calls (one per pack stream) —
   ideally in parallel since ReadAt is stateless.
3. Pipe all 4 streams through the BCJ2 decoder.

The ReadAt interface does NOT need to change. This is a format-parser
concern, not an I/O concern.

### External Properties (rare but real)

Some archives store file metadata (including names) outside the main
header in `AdditionalStreamsInfo` (property ID `0x03`) with an
`External == 0x01` flag. The parser must detect this flag and decompress
the additional stream during `open()` to build a complete file listing.
Adds one more HTTP round trip and decompression to the open cost.

**Liberal reader / strict writer rule:** Property IDs within a 7z header
MUST be accepted in any order (many real-world tools violate the spec's
ascending-order requirement). Always emit properties in ascending order
when writing.

---

## ZIP Filename Encoding

- An explicit `ArchiveConfig::filename_encoding` overrides all ZIP filename flags
  and detection. `auto` is the default; `cp932` aliases Shift_JIS.
- In auto mode, GP bit 11 set: decode as UTF-8; do not feed it to detection.
- Bit 11 clear: feed raw filenames, separated by newlines, to `chardetng`.
  Guess one encoding for the archive with UTF-8 allowed and no domain hint,
  then decode unflagged names using `encoding_rs` without BOM handling.

Detection uses the already-loaded Central Directory, with no additional I/O.
ZIP disk-cache hits replay the original metadata through this same parser, so
filename decoding is rerun on every open.
This supports legacy Japanese Shift_JIS/CP932 names as well as other encodings
supported by chardetng. Explicit UTF-8 entries take precedence in mixed archives.

Detection is heuristic: short names and archives mixing multiple unflagged
encodings can be misidentified. CP437 is not supported by chardetng/encoding_rs;
Windows-1252 is not a CP437 substitute. Unicode Path extra fields are not currently implemented.
The global CLI `--encoding` option applies to ZIP filenames in list/get/test and
TUI flows; it does not transcode file contents or change RAR filename decoding.
Encoding overrides are session configuration, never persisted in the raw cache.

**RAR/7z:** UTF-16LE throughout — no encoding ambiguity.
**TAR (pax headers):** UTF-8 with pax extension for long names; legacy
GNU TAR is locale-dependent, treat as lossy UTF-8.
