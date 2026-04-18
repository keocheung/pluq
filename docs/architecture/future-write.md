# Write-Side Notes (post-v1)

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.
>
> v1 scope is read-only extraction. Write support is post-v1 (see
> `ROADMAP.md` § Future Ideas). Read this only when working on archive
> creation.

---

## 7z: Header Written at End

ZIP writes metadata inline with each entry (local header → data, repeat).
You can stream-write a ZIP.

**7z is structurally inverted**: the header block (containing pack sizes,
offsets, CRCs, method info for all entries) is written at the **end** of
the file. You cannot finalize a 7z archive until the very last byte of
compressed data is known.

**Implication for Vault / cloud archiving:** A 7z archive must be built
entirely locally before upload. There is no "compress-and-stream-to-cloud-
simultaneously" for 7z. ZIP can pipeline; 7z cannot. Factor this into the
Vault feature design.

---

## User Decision Model (Write Side)

For creating archives, users get three choices:

### Choice 1: Compression Profile (human-readable, not algorithm names)

- **"Most Compatible"** *(default)* → ZIP + Deflate. Opens everywhere
  (WinRAR, 7-Zip, macOS, phone apps). Decent ratio.
- **"Best Compression"** → 7z + LZMA2. Smallest files. Slower. Needs
  7-Zip or compatible tool to open.
- **"Fast"** → ZIP + ZSTD or 7z + ZSTD. Fast encode/decode, good ratio.
  Needs modern tools.

Users never see "Deflate" or "LZMA2". They see the tradeoff that matters
to them.

### Choice 2: Password

Yes/no + passphrase. Encryption method is automatic per format (AES-256
for ZIP, AES-256-SHA256 for 7z).

### Choice 3: (nothing else for now)

That's it. Two choices. Advanced mode (future) exposes: raw algorithm,
compression level (1–9), filter selection (BCJ for executables), solid
block settings, split archive size.
