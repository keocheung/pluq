/// Dev scratchpad — real-world testing scenarios.
///
/// Run: cargo run -p archive-dev
///
/// What this does:
///   1. Lists ALL files from the Dropbox ZIP and saves to output/listing.txt
///   2. Prompts you to enter a file name from the listing
///   3. Extracts that file to output/extracted/
///
/// The output/ folder is gitignored — dump whatever you like there.
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

// ── Real-world test URLs ─────────────────────────────────────────────

/// Stefan's Dropbox test ZIP (7.5 GB, ZIP64, ~13,000 PDFs).
/// dl=1 is required for direct byte access (HTTP Range requests).
const DROPBOX_ZIP: &str = "https://www.dropbox.com/scl/fi/REDACTED/archive.zip?rlkey=REDACTED&st=REDACTED&dl=1";

const LISTING_FILE: &str = "crates/archive-dev/output/listing.txt";
const EXTRACTED_DIR: &str = "crates/archive-dev/output/extracted";

fn main() {
    fs::create_dir_all(EXTRACTED_DIR).expect("failed to create output dirs");

    println!("=== archive-dev: Dropbox ZIP smoke test ===\n");

    // Step 1: list all files and save to output/listing.txt
    let listing_path = Path::new(LISTING_FILE);
    if listing_path.exists() {
        println!("→ Listing file already exists at {LISTING_FILE}");
        println!("  (delete it to re-fetch)\n");
    } else {
        println!("→ Fetching full file listing from Dropbox...");
        println!("  (this may take a moment for a 7.5 GB ZIP)\n");

        let output = run_cli(&["list", DROPBOX_ZIP]);

        if !output.status.success() {
            eprintln!(
                "❌ CLI list failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        fs::write(LISTING_FILE, stdout.as_bytes()).expect("failed to write listing file");
        println!("  ✅ Saved listing to {LISTING_FILE}\n");
    }

    // Step 2: print summary and prompt for a file to extract
    let listing = fs::read_to_string(LISTING_FILE).expect("failed to read listing file");
    let lines: Vec<&str> = listing.lines().collect();

    // Count non-directory file lines (lines with actual sizes)
    let file_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| {
            !l.trim().is_empty()
                && !l.trim().starts_with('-')
                && !l.trim().starts_with("SIZE")
                && !l.trim().starts_with("----")
        })
        .collect();
    println!(
        "→ Listing has {} lines ({} entries). Saved at:\n  {LISTING_FILE}\n",
        lines.len(),
        file_lines.len()
    );
    println!("  Open the listing file to browse files, then enter a name to extract.\n");

    // Step 3: interactive extract loop
    loop {
        print!("Enter file name to extract (or 'q' to quit): ");
        io::stdout().flush().unwrap();

        let stdin = io::stdin();
        let entry_name = stdin
            .lock()
            .lines()
            .next()
            .unwrap()
            .unwrap_or_default()
            .trim()
            .to_string();

        if entry_name == "q" || entry_name.is_empty() {
            println!("Bye!");
            break;
        }

        println!("\n→ Extracting '{}' to {EXTRACTED_DIR}/...", entry_name);

        let output = run_cli(&["extract", DROPBOX_ZIP, &entry_name, "-o", EXTRACTED_DIR]);

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            println!("{}", stdout.trim());
            println!();
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("❌ Extraction failed:\n{stderr}");
        }
    }
}

/// Run a `cargo run -p archive-cli -- <args>` command from the workspace root.
fn run_cli(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "archive-cli", "--"]);
    cmd.args(args);
    cmd.output().expect("failed to run archive-cli")
}
