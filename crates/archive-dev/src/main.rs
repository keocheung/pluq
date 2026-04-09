/// Dev helper — interactive Google Drive test launcher.
///
/// Run: cargo run -p archive-dev
///
use std::io::{self, Write};
use std::process::Command;

fn main() {
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn prompt(msg: &str) -> String {
    print!("{msg}");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).expect("failed to read line");
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
