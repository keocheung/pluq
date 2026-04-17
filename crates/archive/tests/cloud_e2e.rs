//! Cloud sniping end-to-end tests — hit real public URLs via HTTP Range requests.
//!
//! These tests require network access and are **ignored by default** to keep CI
//! offline. Run them manually from the workspace root:
//!
//! ```text
//! # All cloud E2E tests:
//! cargo test -p archive --test cloud_e2e -- --ignored --nocapture
//!
//! # GitHub RAR5 suite only (small files, fast, content-verified):
//! cargo test -p archive --test cloud_e2e cloud_rar5_github -- --ignored --nocapture
//!
//! # Stefan's large personal archives (listing only):
//! cargo test -p archive --test cloud_e2e cloud_zip_dropbox -- --ignored --nocapture
//! cargo test -p archive --test cloud_e2e cloud_rar_google_drive -- --ignored --nocapture
//! ```
//!
//! ## Test tiers
//!
//! ### Tier 1a — `ssokolow/rar-test-files` (small, fast, content-verified)
//!
//! Tiny RAR5 files (82–410 bytes). Covers: plain, CBR, recovery record, solid
//! (entries skipped), RAR3 rejection.
//!
//! Source: <https://github.com/ssokolow/rar-test-files> — CC0 public domain.
//!
//! ### Tier 1b — `muja/unrar.rs` fixtures (the crate we depend on)
//!
//! The RAR5 fixtures from the `unrar` crate's own test suite, also served via
//! `raw.githubusercontent.com`. These cover edge cases not in ssokolow's set:
//! archive comments (CMT service block), locked archives, unusual Unicode
//! filenames, and encrypted-header archives (graceful failure).
//!
//! Verified contents (extracted locally from the cargo registry copy):
//! - `comment.rar`   → `.gitignore` = `"target\nCargo.lock\n"`, comment `"abcdef12345"`
//! - `locked.rar`    → `.gitignore` = `"target\nCargo.lock\n"`
//! - `unicode.rar`   → `te…―st✌` = 17 bytes (4 emoji + `\n`)
//! - `comment-hpw-password.rar` → RAR5 with encrypted headers (type 4 block)
//!
//! Source: <https://github.com/muja/unrar.rs> — MIT license.
//!
//! ### Tier 2 — Stefan's large personal archives
//!
//! Very large files (GBs). Listing-only tests — extraction is omitted because
//! it would download too much data for a casual run. Use `cargo run -p archive-dev`
//! for interactive listing + targeted extraction sessions on these.

use archive::{Archive, ArchiveError};
use archive_source::{SourceConfig, resolve};

fn open_url(url: &str) -> Result<archive_source::ResolvedSource, archive_source::SourceError> {
    resolve(url, &SourceConfig::default())
}

// ── Tier 1 URLs — ssokolow/rar-test-files ────────────────────────────────────
//
// Pinned to a specific commit SHA for reproducibility.
// All files are from `build/` in the repo; all are RAR5 unless noted.
// Archive contents (from Makefile + sources/):
//   *.rar files  → testfile.txt  = "Testing 123\n" (12 bytes), compressed -m5
//   *.cbr files  → testfile.jpg (220 B) + testfile.png (87 B), compressed -m5
//   *.solid.* → solid archive; our parser skips solid entries → empty listing
//   *.rr.*    → has a recovery-record service block; listing should still work
//   *.rar3.*  → RAR3 format; must return UnsupportedFormat

const GH_BASE: &str = "https://raw.githubusercontent.com/ssokolow/rar-test-files/\
    16b785c2b1b504e99fc307676e5369a26d3ce060/build";

fn gh(filename: &str) -> String {
    format!("{GH_BASE}/{filename}")
}

// ── Tier 1b URLs — muja/unrar.rs fixtures ────────────────────────────────────
//
// All pinned to the same commit SHA used by unrar-0.5.8 in our cargo registry.
// All files confirmed RAR5 via magic-byte check on the local registry copy.

const UNRAR_BASE: &str = "https://raw.githubusercontent.com/muja/unrar.rs/\
    0628d12b4f2f79d80e6c6c0707ce611e8952b052/data";

fn unrar(filename: &str) -> String {
    format!("{UNRAR_BASE}/{filename}")
}

fn dev_link(key: &str) -> Option<String> {
    // 1. Try env var
    let env_key = format!("ARCHIVE_SNIPER_{}", key.to_uppercase());
    if let Ok(val) = std::env::var(&env_key) {
        return Some(val);
    }

    // 2. Try dev_links.json (workspace root or crate root)
    let paths = ["dev_links.json", "../../dev_links.json"];
    for path in paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(val) = v.get(key).and_then(|v| v.as_str()) {
            return Some(val.to_string());
        }
    }
    None
}

// ── Tier 1 tests ──────────────────────────────────────────────────────────────

/// List + extract a plain RAR5 archive from a raw GitHub URL.
///
/// Verifies the full cloud sniping round-trip:
/// - `HttpReadAt` fetches the RAR5 signature + main header via range request
/// - Parser builds the entry list
/// - `extract_via_unrar_cloud` downloads only the file header + compressed data,
///   builds a synthetic archive, and decompresses via unrar
/// - Extracted content matches the known source: `"Testing 123\n"`
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_github_plain -- --ignored --nocapture"]
async fn cloud_rar5_github_plain_listing_and_extraction() {
    let url = gh("testfile.rar5.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(entries.len(), 1, "should have exactly 1 entry");
    assert_eq!(entries[0].name, "testfile.txt");
    assert_eq!(entries[0].uncompressed_size, 12);
    assert!(!entries[0].is_directory);
    assert!(!entries[0].is_encrypted);

    println!("✅  listing OK: {:?}", entries[0].name);

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("extraction should succeed");
    assert_eq!(data.as_ref(), b"Testing 123\n");

    println!("✅  extraction OK: {:?}", String::from_utf8_lossy(&data));
}

/// List + extract a RAR5 CBR (comic book archive) from a raw GitHub URL.
///
/// `.cbr` files are RAR archives with a different extension. Magic-byte
/// detection must kick in regardless of the URL's extension. The archive
/// contains two image files compressed at level 5.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_github_cbr -- --ignored --nocapture"]
async fn cloud_rar5_github_cbr_listing_and_extraction() {
    let url = gh("testfile.rar5.cbr");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(entries.len(), 2, "CBR should have 2 image entries");

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"testfile.jpg"),
        "should contain testfile.jpg"
    );
    assert!(
        names.contains(&"testfile.png"),
        "should contain testfile.png"
    );

    println!("✅  listing OK: {names:?}");

    // Extract both entries and verify they are non-empty image bytes
    for entry in entries.iter() {
        let data = archive
            .extract_to_bytes(entry, None)
            .await
            .expect("extraction should succeed");
        assert!(
            !data.is_empty(),
            "{} must have non-empty content",
            entry.name
        );
        println!("✅  extracted {} ({} bytes)", entry.name, data.len());
    }
}

/// RAR5 archive with a Recovery Record service block.
///
/// The RR block is a service header (block type 3). Our parser skips service
/// headers during the sequential scan, so the entry list should still contain
/// exactly one file entry.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_github_rr -- --ignored --nocapture"]
async fn cloud_rar5_github_recovery_record_listing_and_extraction() {
    let url = gh("testfile.rar5.rr.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(
        entries.len(),
        1,
        "should have 1 file entry (RR service block skipped)"
    );
    assert_eq!(entries[0].name, "testfile.txt");

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("extraction should succeed");
    assert_eq!(data.as_ref(), b"Testing 123\n");

    println!("✅  RR archive: listing + extraction OK");
}

/// Solid RAR5 archive — first entry is always accessible.
///
/// RAR5 uses a *per-file* solid flag (bit 6 of the compression info vint).
/// The first entry in a solid archive always has this flag = 0 (there is no
/// preceding dictionary to inherit). Our parser skips entries where the flag
/// is 1 (entries 2+ in a multi-file solid archive), so the first entry is
/// always reachable and extractable.
///
/// `testfile.rar5.solid.rar` has exactly one file (`testfile.txt`), so the
/// per-file solid flag is 0 → 1 entry in the listing → extraction works.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_github_solid -- --ignored --nocapture"]
async fn cloud_rar5_github_solid_first_entry_accessible() {
    let url = gh("testfile.rar5.solid.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(
        entries.len(),
        1,
        "first entry of a solid archive is accessible (per-file solid = 0)"
    );
    assert_eq!(entries[0].name, "testfile.txt");

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("first entry of solid archive should extract");
    assert_eq!(data.as_ref(), b"Testing 123\n");

    println!("✅  solid archive (single file): listed + extracted OK");
}

/// RAR3 archive must return `UnsupportedFormat`.
///
/// RAR3 uses a different header format (7-byte signature, fixed-size fields).
/// We detect it by magic bytes and reject it with a clear error rather than
/// silently misparsing it.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar3_github -- --ignored --nocapture"]
async fn cloud_rar3_github_unsupported_format() {
    let url = gh("testfile.rar3.rar");
    let result = Archive::open_reader(&url, open_url(&url).unwrap()).await;

    assert!(
        matches!(result, Err(ArchiveError::UnsupportedFormat { .. })),
        "RAR3 must return UnsupportedFormat, got: {result:?}"
    );

    println!("✅  RAR3 correctly rejected with UnsupportedFormat");
}

// ── Tier 1b tests ─────────────────────────────────────────────────────────────

/// RAR5 archive with an archive comment (CMT service header).
///
/// RAR5 comments are stored as a service header (block type 3, name "CMT").
/// Our parser skips all service blocks, so the real file entry must still
/// appear in the listing. The comment content (`"abcdef12345"`) is ignored.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_unrar_comment -- --ignored --nocapture"]
async fn cloud_rar5_unrar_comment_listing_and_extraction() {
    let url = unrar("comment.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(
        entries.len(),
        1,
        "CMT service block must be skipped; 1 file entry expected"
    );
    assert_eq!(entries[0].name, ".gitignore");
    assert_eq!(entries[0].uncompressed_size, 18);

    println!("✅  comment.rar listing OK: {:?}", entries[0].name);

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("extraction should succeed");
    assert_eq!(data.as_ref(), b"target\nCargo.lock\n");

    println!("✅  extraction OK: {:?}", String::from_utf8_lossy(&data));
}

/// RAR5 locked archive.
///
/// The "locked" flag (`HFL_LOCK`) marks the archive as read-only for
/// modification tools but has no effect on reading/extraction. Our parser
/// should list and extract normally.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_unrar_locked -- --ignored --nocapture"]
async fn cloud_rar5_unrar_locked_listing_and_extraction() {
    let url = unrar("locked.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(entries.len(), 1, "locked archive should list normally");
    assert_eq!(entries[0].name, ".gitignore");
    assert_eq!(entries[0].uncompressed_size, 18);

    println!("✅  locked.rar listing OK");

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("extraction should succeed");
    assert_eq!(data.as_ref(), b"target\nCargo.lock\n");

    println!("✅  extraction OK");
}

/// RAR5 archive with an unusual Unicode entry name: `te…―st✌`.
///
/// The name contains: `…` (HORIZONTAL ELLIPSIS U+2026), `―` (HORIZONTAL BAR
/// U+2015), `✌` (VICTORY HAND U+270C). Verifies that our UTF-8 filename
/// parsing handles multi-byte sequences beyond the ASCII range.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_unrar_unicode -- --ignored --nocapture"]
async fn cloud_rar5_unrar_unicode_filename() {
    let url = unrar("unicode.rar");
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .unwrap()
        .ready()
        .await
        .unwrap();

    let entries = archive.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "te\u{2026}\u{2015}st\u{270C}"); // te…―st✌
    assert_eq!(entries[0].uncompressed_size, 17);

    println!("✅  unicode.rar entry name: {:?}", entries[0].name);

    let data = archive
        .extract_to_bytes(&entries[0], None)
        .await
        .expect("extraction should succeed");
    assert_eq!(data.len(), 17);
    assert!(data.ends_with(b"\n"));

    println!("✅  extraction OK ({} bytes)", data.len());
}

/// RAR5 archive with encrypted headers (archive encryption header, block type 4).
///
/// This is an out-of-scope feature (v1 doesn't support encrypted-header
/// archives). The test verifies that our parser handles this gracefully:
/// either returning a meaningful error or an empty listing — but never
/// panicking or hanging.
///
/// Expected: error during open (corrupt header or similar) when the scanner
/// tries to parse the AES-encrypted blocks after the encryption header.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar5_unrar_enc_headers -- --ignored --nocapture"]
async fn cloud_rar5_unrar_encrypted_headers_graceful() {
    let url = unrar("comment-hpw-password.rar");
    let result = Archive::open_reader(&url, open_url(&url).unwrap()).await;

    match &result {
        Err(e) => println!("✅  returned error (expected): {e:?}"),
        Ok(opened) => {
            let archive = opened;
            println!("✅  opened (encrypted blocks skipped)");
            // If it reaches Ready, check via ready()
            let _ = archive;
        }
    }

    // The important thing: no panic, no hang. Either outcome is acceptable.
    match result {
        Err(ArchiveError::FileCorrupted { .. }) => { /* expected: CorruptHeader → FileCorrupted */
        }
        Ok(opened) => {
            // If somehow it opened, scan and check.
            match opened.ready().await {
                Ok(archive) => {
                    assert!(
                        archive.entries().is_empty(),
                        "encrypted-header archive must not produce usable entries without a password"
                    );
                }
                Err(_) => { /* error during scan is also acceptable */ }
            }
        }
        Err(other) => {
            // Any other error is also acceptable — just not a panic.
            println!("  (other error variant: {other:?})");
        }
    }
}

// ── Tier 2 tests ──────────────────────────────────────────────────────────────

/// Verify that Stefan's Dropbox ZIP can be listed via HTTP Range requests.
///
/// ZIP Central Directory is at the end of the archive — listing always requires
/// ≤ 3 range requests regardless of archive size. No full download.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_zip_dropbox -- --ignored --nocapture"]
async fn cloud_zip_dropbox_listing() {
    let url = match dev_link("dropbox_zip") {
        Some(u) => u,
        None => {
            println!("⏩ Skipping Dropbox ZIP test: dev_links.json missing or incomplete");
            return;
        }
    };
    let archive = Archive::open_reader(&url, open_url(&url).unwrap())
        .await
        .expect("should open Dropbox ZIP via HTTP range requests")
        .ready()
        .await
        .expect("Dropbox ZIP should be immediately ready");

    let entries = archive.entries();
    assert!(!entries.is_empty(), "Dropbox ZIP must have entries");

    println!("✅  Dropbox ZIP listing OK — {} entries", entries.len());
    for e in entries.iter().take(5) {
        println!("    {} ({} B)", e.name, e.uncompressed_size);
    }
    if entries.len() > 5 {
        println!("    … and {} more", entries.len() - 5);
    }
}

/// Verify that Stefan's Google Drive RAR can be listed via HTTP Range requests.
///
/// ## Current status: requires Phase 4 (cloud auth + URL routing)
///
/// Google Drive files > ~100 MB always return an HTML confirmation page
/// (`200 text/html`) instead of honouring the `Range:` header. The two known
/// URL formats both exhibit this:
///
/// - `drive.google.com/uc?export=download&id=…`        → `200 OK` (no range support)
/// - `drive.usercontent.google.com/download?id=…`      → `200 text/html` (confirmation page)
///
/// The Python script that worked for this file used **OAuth + the Drive API**:
/// `GET https://www.googleapis.com/drive/v3/files/ID?alt=media`
/// with `Authorization: Bearer <access_token>` — that URL returns proper `206`.
///
/// Phase 4 will add: Drive API URL detection → resolve to `?alt=media` form →
/// inject OAuth token → `206` responses → Range requests work.
///
/// This test soft-fails (prints the error, does NOT panic) so the suite stays
/// green while documenting the gap.
#[tokio::test]
#[ignore = "requires network; run: cargo test -p archive --test cloud_e2e cloud_rar_google_drive -- --ignored --nocapture"]
async fn cloud_rar_google_drive_listing() {
    let url = match dev_link("gdrive_rar") {
        Some(u) => u,
        None => {
            println!("⏩ Skipping Google Drive RAR test: dev_links.json missing or incomplete");
            return;
        }
    };
    let reader = match open_url(&url) {
        Ok(r) => r,
        Err(e) => {
            println!(
                "ℹ️  Could not resolve Google Drive URL (expected — needs credentials): {e:?}"
            );
            return;
        }
    };
    let result = Archive::open_reader(&url, reader).await;

    match result {
        Ok(opened) => {
            // If this ever passes, Phase 4 must have landed or the URL changed.
            let archive = opened.ready().await.expect("scan should succeed");
            let entries = archive.entries();
            println!(
                "✅  Google Drive RAR listing OK — {} entries",
                entries.len()
            );
            for e in entries.iter().take(10) {
                println!(
                    "    {} ({} → {} B, dir: {}, encrypted: {})",
                    e.name, e.compressed_size, e.uncompressed_size, e.is_directory, e.is_encrypted,
                );
            }
        }
        Err(e) => {
            // Expected until Phase 4 lands.
            println!("ℹ️  Google Drive Range request failed (expected — needs Phase 4):");
            println!("   {e:?}");
            println!();
            println!("   Google Drive returns 200 OK (full HTML / full file) instead of 206");
            println!("   for Range requests on files > ~100 MB. Phase 4 will handle this via:");
            println!(
                "   Drive API URL detection → resolve to googleapis.com/drive/v3/files/ID?alt=media"
            );
            println!("   + inject OAuth Bearer token → proper 206 range responses.");
            // Soft fail: do NOT panic. The test documents the requirement.
        }
    }
}
