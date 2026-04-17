use std::io::{self, Read, Write};
use std::process::Command;
use reqwest::blocking::Client;
use serde_json::json;

fn main() {
    println!("=== 🎯 Archive Sniper: Dev Scratchpad ===\n");
    println!("1. Google Drive Test Launcher (existing)");
    println!("2. Dropbox API PoC (new)");
    
    let choice = prompt("Select an option (1/2): ");
    match choice.trim() {
        "1" => run_gdrive_launcher(),
        "2" => run_dropbox_poc(),
        _ => println!("❌ Invalid choice"),
    }
}

// ── Google Drive Launcher (Old Code) ───────────────────────────────────────────

fn run_gdrive_launcher() {
    println!("=== 🎯 Archive Sniper: Google Drive Test Launcher ===\n");

    // 0. Load default URLs from dev_links.json
    let dev_links = match std::fs::read_to_string("dev_links.json") {
        Ok(s) => serde_json::from_str::<serde_json::Value>(&s).ok(),
        Err(_) => None,
    };

    let default_gdrive_url = dev_links
        .as_ref()
        .and_then(|v| v.get("default_gdrive_url"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://drive.google.com/");

    // 1. Check for API Key (Env -> .env file -> Prompt)
    let mut api_key = std::env::var("ARCHIVE_SNIPER_GDRIVE_KEY").ok();

    if api_key.is_none() {
        // Try reading from .env file
        if let Ok(content) = std::fs::read_to_string(".env") {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("ARCHIVE_SNIPER_GDRIVE_KEY=") {
                    api_key = Some(val.trim().trim_matches('\"').to_string());
                    break;
                }
            }
        }
    }

    if api_key.is_none() {
        println!("⚠️  ARCHIVE_SNIPER_GDRIVE_KEY is not set.");
        let k = prompt("🔑 Enter your Google Drive API Key: ");
        let k = k.trim().to_string();
        if k.is_empty() {
            println!("❌ Error: API key is required.");
            return;
        }

        // Save to .env file for next time
        println!("💾 Saving key to .env file...");
        let _ = std::fs::write(".env", format!("ARCHIVE_SNIPER_GDRIVE_KEY=\"{k}\"\n"));
        api_key = Some(k);
    } else {
        println!("✅ API Key found.");
    }

    // 2. Ask for URL
    println!("\n📂 Default URL: {}", default_gdrive_url);
    let url_input = prompt("🔗 Enter GDrive URL (or press Enter for default): ");
    let url = if url_input.trim().is_empty() {
        default_gdrive_url.to_string()
    } else {
        url_input.trim().to_string()
    };

    // 3. Optional Flags
    let use_debug = prompt_bool("🐛 Enable --debug logging? (y/n): ", true);
    let use_clean = prompt_bool("🧹 Enable --clean-cache? (y/n): ", true);

    // 4. Build and Run Command
    println!("\n🚀 Launching TUI Browse...");

    let mut args = vec!["run", "-p", "archive-cli", "--"];
    if use_debug {
        args.push("--debug");
    }
    if use_clean {
        args.push("--clean-cache");
    }
    args.push("browse");
    args.push(&url);

    let mut cmd = Command::new("cargo");
    cmd.args(&args);

    // Set the env var for the child process
    if let Some(key) = api_key {
        cmd.env("ARCHIVE_SNIPER_GDRIVE_KEY", key);
    }

    // Inherit stdin/stdout/stderr so the TUI works correctly
    let status = cmd.status().expect("failed to execute archive-cli");

    if !status.success() {
        eprintln!("\n❌ TUI exited with error code: {}", status);
    }
}

// ── Dropbox PoC (New Code) ───────────────────────────────────────────────────

fn run_dropbox_poc() {
    println!("=== 🎯 Archive Sniper: Dropbox API PoC ===\n");

    let dev_links = match std::fs::read_to_string("dev_links.json") {
        Ok(s) => serde_json::from_str::<serde_json::Value>(&s).ok(),
        Err(_) => None,
    };

    let personal_url = dev_links
        .as_ref()
        .and_then(|v| v.get("dropbox_zip"))
        .and_then(|v| v.as_str());

    let public_url = dev_links
        .as_ref()
        .and_then(|v| v.get("dropbox_public_zip"))
        .and_then(|v| v.as_str());

    if let Some(url) = personal_url { println!("1. Personal: {}", url); }
    if let Some(url) = public_url { println!("2. Public:   {}", url); }
    println!("3. Custom URL");

    let choice = prompt("Select a link option (1/2/3): ");
    let url = match choice.trim() {
        "1" => personal_url.map(|s| s.to_string()).unwrap_or_default(),
        "2" => public_url.map(|s| s.to_string()).unwrap_or_default(),
        _ => prompt("🔗 Enter Dropbox Shared Link: ").trim().to_string(),
    };
    
    if url.is_empty() {
        println!("❌ Error: URL is required.");
        return;
    }

    // Try to get token from env
    let mut token = std::env::var("ARCHIVE_SNIPER_DROPBOX_TOKEN").ok();

    if token.is_none() {
        if let Ok(content) = std::fs::read_to_string(".env") {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("ARCHIVE_SNIPER_DROPBOX_TOKEN=") {
                    token = Some(val.trim().trim_matches('\"').to_string());
                    break;
                }
            }
        }
    }

    if token.is_none() {
        println!("⚠️  ARCHIVE_SNIPER_DROPBOX_TOKEN is not set.");
        let t = prompt("🔑 Enter your Dropbox Access Token (or press Enter to try without): ");
        let t = t.trim().to_string();
        if !t.is_empty() {
             token = Some(t);
        }
    }
    
    let client = Client::new();
    
    // Step 1: Get Metadata (Size)
    println!("\n📊 1. Fetching Metadata...");
    let meta_url = "https://api.dropboxapi.com/2/sharing/get_shared_link_metadata";
    let mut meta_req = client.post(meta_url)
        .header("Content-Type", "application/json")
        .json(&json!({ "url": url }));

    if let Some(ref t) = token {
        meta_req = meta_req.header("Authorization", format!("Bearer {}", t));
    }

    let meta_resp = meta_req.send().expect("failed to send metadata request");
    println!("📡 Metadata Status: {}", meta_resp.status());
    
    let meta_body: serde_json::Value = meta_resp.json().expect("failed to parse metadata JSON");
    if meta_body.get("error").is_some() {
        println!("❌ Metadata Error: {}", meta_body);
        return;
    }
    
    let size = meta_body.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let name = meta_body.get("name").and_then(serde_json::Value::as_str).unwrap_or("unknown");
    println!("✅ Name: {}, Size: {} bytes", name, size);

    // Step 2: Surgical Read (Range)
    println!("\n🎯 2. Performing Surgical Read (Range: bytes=0-19)...");
    let dl_url = "https://content.dropboxapi.com/2/sharing/get_shared_link_file";
    let mut dl_req = client.post(dl_url)
        .header("Dropbox-API-Arg", json!({ "url": url }).to_string())
        .header("Range", "bytes=0-19");

    if let Some(ref t) = token {
        dl_req = dl_req.header("Authorization", format!("Bearer {}", t));
    }

    let mut dl_resp = dl_req.send().expect("failed to send download request");
    println!("📡 Download Status: {}", dl_resp.status());
    
    if dl_resp.status().is_success() {
        let mut buffer = Vec::new();
        dl_resp.read_to_end(&mut buffer).expect("failed to read body");
        println!("✅ Received {} bytes.", buffer.len());
        println!("💾 Bytes (hex): {:02x?}", buffer);
        
        if buffer.starts_with(&[0x50, 0x4b]) {
            println!("📂 Signature: ZIP (PK) detected!");
        }
    } else {
        let mut err_body = String::new();
        dl_resp.read_to_string(&mut err_body).ok();
        println!("❌ Download Error: {}", err_body);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn prompt(msg: &str) -> String {
    print!("{msg}");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .expect("failed to read line");
    input
}

fn prompt_bool(msg: &str, default: bool) -> bool {
    let input = prompt(msg);
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return default;
    }
    input == "y" || input == "yes"
}
