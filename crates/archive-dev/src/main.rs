/// Dev scratchpad — real-world cloud sniping scenarios.
///
/// Run: cargo run -p archive-dev
///
/// Scenarios:
///   1) Dropbox ZIP  — list all files (saved to output/zip_listing.txt),
///      then prompt for interactive extraction.
///   2) Google Drive RAR — list entries only (saved to output/rar_listing.txt).
///      Extraction is available but the archive is very large;
///      test with a specific known entry name.
///
/// The output/ folder is gitignored — dump whatever you like there.
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

// ── Real-world test URLs ──────────────────────────────────────────────────────

/// Stefan's Dropbox ZIP (~7.5 GB, ZIP64, ~13 000 PDFs).
/// `dl=1` is required for direct byte access (HTTP Range requests).
const DROPBOX_ZIP: &str = "https://www.dropbox.com/scl/fi/REDACTED/\
    archive.zip?rlkey=REDACTED&st=xkx9j6il&dl=1";

/// Stefan's Google Drive RAR (RAR5, very large).
/// `uc?export=download&id=<id>` is the direct-download endpoint;
/// HTTP Range requests for headers work without needing a confirmation cookie.
const GOOGLE_DRIVE_RAR: &str =
    "https://drive.google.com/uc?export=download&id=1ExampleGDriveFileId000000000000";

const ZIP_LISTING_FILE: &str = "crates/archive-dev/output/zip_listing.txt";
const RAR_LISTING_FILE: &str = "crates/archive-dev/output/rar_listing.txt";
const EXTRACTED_DIR: &str = "crates/archive-dev/output/extracted";

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    fs::create_dir_all(EXTRACTED_DIR).expect("failed to create output dirs");

    println!("=== archive-dev: Cloud Sniping Smoke Tests ===\n");
    println!("  1) Dropbox ZIP  — list & extract  (~7.5 GB, ~13 000 PDFs)");
    println!("  2) Google Drive RAR — list only   (very large RAR5)");
    println!("  3) Phase 4 Gap — Dropbox range support (quick check)");
    println!("  q) Quit\n");

    let choice = prompt("Choose scenario: ");
    match choice.trim() {
        "1" => run_zip_scenario(),
        "2" => run_rar_scenario(),
        "3" => run_dropbox_gap_check(),
        _ => println!("Bye!"),
    }
}

// ── Scenario 3: Phase 4 Gap Check ─────────────────────────────────────────────

fn run_dropbox_gap_check() {
    println!("\n=== Scenario 3: Phase 4 Gap Check (Dropbox API) ===\n");
    println!("Verifying if the stable Dropbox API endpoint supports HTTP Range requests");
    println!("using App Credentials (Basic Auth) for publicly shared files.\n");

    let app_key = match std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_KEY") {
        Ok(k) => k,
        Err(_) => {
            println!("❌ Error: ARCHIVE_SNIPER_DROPBOX_APP_KEY not set.");
            println!("   Please set it to your Dropbox App Key.");
            return;
        }
    };
    let app_secret = match std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET") {
        Ok(s) => s,
        Err(_) => {
            println!("❌ Error: ARCHIVE_SNIPER_DROPBOX_APP_SECRET not set.");
            println!("   Please set it to your Dropbox App Secret.");
            return;
        }
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent("archive-sniper-dev")
        .build()
        .expect("failed to build client");

    let url = "https://content.dropboxapi.com/2/sharing/get_shared_link_file";
    let arg = serde_json::json!({ "url": DROPBOX_ZIP });
    let arg_header = serde_json::to_string(&arg).expect("failed to serialize arg");

    use base64::prelude::*;
    let basic_auth = BASE64_STANDARD.encode(format!("{app_key}:{app_secret}"));

    println!("→ Sending POST to {url}…");
    println!("  Headers:");
    println!("    Authorization: Basic [REDACTED]");
    println!("    Dropbox-API-Arg: {arg_header}");
    println!("    Range: bytes=0-15");
    println!();

    let resp = client.post(url)
        .header("Authorization", format!("Basic {basic_auth}"))
        .header("Dropbox-API-Arg", &arg_header)
        .header("Range", "bytes=0-15")
        .body("")
        .send()
        .expect("POST request failed");

    println!("  Status: {}", resp.status());
    println!("  Content-Range: {:?}", resp.headers().get("content-range"));
    println!("  Content-Length: {:?}", resp.headers().get("content-length"));
    println!("  Dropbox-API-Result: {:?}", resp.headers().get("dropbox-api-result"));

    if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let body = resp.bytes().expect("failed to read body");
        println!("\n  Received {} bytes of body content.", body.len());
        
        let hex = body.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
        println!("  Body hex: {hex}");

        if body.starts_with(b"PK\x03\x04") {
            println!("\n  ✅ SUCCESS! Received correct ZIP magic bytes (PK\\x03\\x04).");
            println!("     The stable Dropbox API endpoint supports surgical extraction via Range");
            println!("     using App Credentials without OAuth.");
        } else {
            println!("\n  ❌ FAILED! Body does not start with ZIP magic bytes.");
        }
    } else {
        println!("\n  ❌ FAILED! Expected 206 Partial Content, got {}", resp.status());
        if let Ok(text) = resp.text() {
            println!("  Response body: {text}");
        }
    }
    println!();
}

// ── Scenario 1: Dropbox ZIP ───────────────────────────────────────────────────

fn run_zip_scenario() {
    println!("\n=== Scenario 1: Dropbox ZIP ===\n");

    // Step 1: list all files, cache to disk
    let listing_path = Path::new(ZIP_LISTING_FILE);
    if listing_path.exists() {
        println!("→ Listing already cached at {ZIP_LISTING_FILE}");
        println!("  (delete it to re-fetch)\n");
    } else {
        println!("→ Fetching full file listing from Dropbox…");
        println!("  (ZIP Central Directory is at end of file — only a few KB fetched)\n");

        let output = run_cli(&["list", DROPBOX_ZIP]);
        if !output.status.success() {
            eprintln!("❌ CLI list failed:\n{}", String::from_utf8_lossy(&output.stderr));
            return;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        fs::write(ZIP_LISTING_FILE, stdout.as_bytes()).expect("failed to write listing");
        println!("  ✅ Saved listing to {ZIP_LISTING_FILE}\n");
    }

    // Step 2: print summary
    let listing = fs::read_to_string(ZIP_LISTING_FILE).expect("failed to read listing");
    let file_lines: Vec<&str> = listing
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('-') && !l.trim().starts_with("SIZE"))
        .collect();
    println!("→ {} entries in listing. Open {ZIP_LISTING_FILE} to browse.\n", file_lines.len());

    // Step 3: interactive extract loop
    loop {
        let entry_name = prompt("Enter file name to extract (or 'q' to quit): ");
        if entry_name.trim() == "q" || entry_name.trim().is_empty() {
            println!("Bye!");
            break;
        }

        println!("\n→ Extracting '{}' to {EXTRACTED_DIR}/…", entry_name.trim());
        let output = run_cli(&["extract", DROPBOX_ZIP, entry_name.trim(), "-o", EXTRACTED_DIR]);
        if output.status.success() {
            println!("{}", String::from_utf8_lossy(&output.stdout).trim());
        } else {
            eprintln!("❌ Extraction failed:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        println!();
    }
}

// ── Scenario 2: Google Drive RAR ──────────────────────────────────────────────

fn run_rar_scenario() {
    println!("\n=== Scenario 2: Google Drive RAR ===\n");
    println!("Note: listing fetches only header bytes (Quick Open block or sequential scan).");
    println!("      The archive is very large — listing may take a moment if no QO block.\n");

    // Step 1: list entries
    let listing_path = Path::new(RAR_LISTING_FILE);
    if listing_path.exists() {
        println!("→ Listing already cached at {RAR_LISTING_FILE}");
        println!("  (delete it to re-fetch)\n");
    } else {
        println!("→ Fetching entry listing from Google Drive RAR…\n");

        let output = run_cli(&["list", GOOGLE_DRIVE_RAR]);
        if !output.status.success() {
            eprintln!("❌ CLI list failed:\n{}", String::from_utf8_lossy(&output.stderr));
            eprintln!("\nIf you see a format error, Google Drive may have returned an HTML page");
            eprintln!("instead of archive bytes (e.g. virus-scan confirmation). This is expected");
            eprintln!("for large files without a confirmation cookie — Phase 4 will handle this.");
            return;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        fs::write(RAR_LISTING_FILE, stdout.as_bytes()).expect("failed to write listing");
        println!("  ✅ Saved listing to {RAR_LISTING_FILE}\n");
    }

    // Step 2: print summary
    let listing = fs::read_to_string(RAR_LISTING_FILE).expect("failed to read listing");
    let file_lines: Vec<&str> = listing
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('-') && !l.trim().starts_with("SIZE"))
        .collect();
    println!("→ {} entries in listing. Open {RAR_LISTING_FILE} to browse.\n", file_lines.len());

    // Step 3: optional extraction (skippable — archive is very large)
    println!("Extraction is available but the archive is very large.");
    println!("Only attempt extraction if you know the entry name and accept the download.\n");

    loop {
        let entry_name = prompt("Enter file name to extract (or 'q' to skip): ");
        if entry_name.trim() == "q" || entry_name.trim().is_empty() {
            println!("Skipping extraction. Bye!");
            break;
        }

        println!("\n→ Extracting '{}' from RAR to {EXTRACTED_DIR}/…", entry_name.trim());
        println!("  (downloads compressed data for this entry only)\n");

        let output = run_cli(&["extract", GOOGLE_DRIVE_RAR, entry_name.trim(), "-o", EXTRACTED_DIR]);
        if output.status.success() {
            println!("{}", String::from_utf8_lossy(&output.stdout).trim());
        } else {
            eprintln!("❌ Extraction failed:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        println!();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run a `cargo run -p archive-cli -- <args>` command from the workspace root.
fn run_cli(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "archive-cli", "--"]);
    cmd.args(args);
    cmd.output().expect("failed to run archive-cli")
}

/// Print a prompt and read one line from stdin.
fn prompt(msg: &str) -> String {
    print!("{msg}");
    io::stdout().flush().unwrap();
    let stdin = io::stdin();
    stdin
        .lock()
        .lines()
        .next()
        .unwrap()
        .unwrap_or_default()
}
