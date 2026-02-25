/// Dropbox ZIP snipe experiment.
///
/// Tests our HttpReadAt + ZipArchive stack against a real Dropbox file.
///
/// Run with: cargo run -p archive-dev
use archive_core::{HttpReadAt, ReadAt, ZipArchive};

// Dropbox: change dl=0 to dl=1 to get a direct download URL.
// The direct URL support HTTP Range requests which is what we need.
const DROPBOX_URL: &str = "https://www.dropbox.com/scl/fi/REDACTED/archive.zip?rlkey=REDACTED&st=REDACTED&dl=1";

#[tokio::main]
async fn main() {
    println!("=== Dropbox ZIP Snipe Test ===\n");
    println!("URL: {}\n", DROPBOX_URL);

    let client = reqwest::Client::builder()
        .user_agent("archive-sniper/dev")
        .build()
        .unwrap();

    let reader = HttpReadAt::new(DROPBOX_URL, client);

    // Step 1: inspect the last ~100 bytes to diagnose EOCD format
    println!("→ Inspecting last 100 bytes for EOCD signatures...");
    {
        let size = 7919744653u64;
        let resp = reqwest::Client::builder()
            .user_agent("archive-sniper/dev")
            .build()
            .unwrap()
            .get(DROPBOX_URL)
            .header("Range", format!("bytes={}-{}", size - 100, size - 1))
            .send()
            .await
            .unwrap();
        let body = resp.bytes().await.unwrap();
        println!("  Last 100 bytes (hex):");
        for (i, chunk) in body.chunks(16).enumerate() {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            println!("  {:04x}: {}", i * 16, hex.join(" "));
        }
        // Check for signatures
        // Standard EOCD:  50 4b 05 06
        // ZIP64 EOCD Locator: 50 4b 06 07
        // ZIP64 EOCD record:  50 4b 06 06
        for i in 0..body.len().saturating_sub(3) {
            let sig = u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
            match sig {
                0x06054b50 => println!("  ✅ Standard EOCD at tail offset -{}", body.len() - i),
                0x07064b50 => {
                    println!("  ✅ ZIP64 EOCD Locator at tail offset -{}", body.len() - i)
                }
                0x06064b50 => println!("  ✅ ZIP64 EOCD Record at tail offset -{}", body.len() - i),
                _ => {}
            }
        }
        println!();
    }

    // Step 2: get file size (HEAD request)
    println!("→ Getting file size...");
    match reader.size().await {
        Ok(size) => println!(
            "  File size: {} bytes ({:.2} MB)",
            size,
            size as f64 / 1_048_576.0
        ),
        Err(e) => {
            eprintln!("  ERROR getting size: {e}");
            return;
        }
    }

    // Step 2: parse the ZIP (EOCD + Central Directory)
    println!("\n→ Parsing ZIP Central Directory...");
    let zip = match ZipArchive::open(reader).await {
        Ok(z) => z,
        Err(e) => {
            eprintln!("  ERROR opening ZIP: {e}");
            return;
        }
    };

    let entries = zip.entries();
    println!("  Found {} entries\n", entries.len());

    // Step 3: list first 20 entries
    println!("=== First 20 entries ===");
    println!("  {:<12} {:<12} {}", "SIZE", "COMPRESSED", "NAME");
    println!("  {:<12} {:<12} {}", "----", "----------", "----");

    for entry in entries.iter().take(20) {
        if entry.is_directory {
            println!("  {:<12} {:<12} [DIR] {}", "-", "-", entry.name);
        } else {
            println!(
                "  {:<12} {:<12} {}",
                format_size(entry.uncompressed_size),
                format_size(entry.compressed_size),
                entry.name,
            );
        }
    }

    if entries.len() > 20 {
        println!("  ... and {} more", entries.len() - 20);
    }

    // Step 4: extract the first non-directory file
    let first_file = entries.iter().find(|e| !e.is_directory);
    if let Some(entry) = first_file {
        println!("\n→ Extracting first file: '{}'", entry.name);
        match zip.extract(entry).await {
            Ok(data) => {
                println!("  ✅ Success! Got {} bytes", data.len());
                // Print first 64 bytes as a sanity check
                let preview = &data[..data.len().min(64)];
                println!("  First {} bytes: {:?}", preview.len(), preview);
            }
            Err(e) => eprintln!("  ❌ ERROR extracting: {e}"),
        }
    }

    println!("\n=== Done ===");
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
