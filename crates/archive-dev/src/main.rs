use reqwest::blocking::Client;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::process::Command;
use std::time::{Duration, Instant};

fn main() {
    println!("=== 🎯 pluq: Dev Scratchpad ===\n");
    println!("1. Google Drive Test Launcher (TUI)");
    println!("2. Dropbox Test Launcher (TUI)");
    println!("3. Dropbox API PoC (Manual)");
    println!("4. Dropbox Range Probe (200/206 flip — full header dump)");

    let choice = prompt("Select an option (1/2/3/4): ");
    match choice.trim() {
        "1" => run_gdrive_launcher(),
        "2" => run_dropbox_launcher(),
        "3" => run_dropbox_poc(),
        "4" => run_dropbox_range_probe(),
        _ => println!("❌ Invalid choice"),
    }
}

// ── Dropbox Launcher (TUI) ───────────────────────────────────────────────────

fn run_dropbox_launcher() {
    println!("=== 🎯 pluq: Dropbox TUI Launcher ===\n");

    // 0. Load default URLs from dev_links.json
    let dev_links = match std::fs::read_to_string(".workspace/dev_links.json")
        .or_else(|_| std::fs::read_to_string("dev_links.json"))
    {
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

    if let Some(url) = personal_url {
        println!("1. Personal: {}", url);
    }
    if let Some(url) = public_url {
        println!("2. Public:   {}", url);
    }
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

    // 1. Check for API Key (Env -> .env file -> Prompt)
    let mut app_key = std::env::var("PLUQ_DROPBOX_APP_KEY").ok();
    let mut app_secret = std::env::var("PLUQ_DROPBOX_APP_SECRET").ok();

    if app_key.is_none() || app_secret.is_none() {
        // Try reading from .env file
        if let Ok(content) = std::fs::read_to_string(".env") {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("PLUQ_DROPBOX_APP_KEY=") {
                    app_key = Some(val.trim().trim_matches('\"').to_string());
                }
                if let Some(val) = line.strip_prefix("PLUQ_DROPBOX_APP_SECRET=") {
                    app_secret = Some(val.trim().trim_matches('\"').to_string());
                }
            }
        }
    }

    if app_key.is_none() || app_secret.is_none() {
        println!("⚠️  Dropbox app credentials are not fully set.");
        if app_key.is_none() {
            let k = prompt("🔑 Enter your Dropbox App Key: ");
            let k = k.trim().to_string();
            app_key = Some(k);
        }
        if app_secret.is_none() {
            let s = prompt("🔑 Enter your Dropbox App Secret: ");
            let s = s.trim().to_string();
            app_secret = Some(s);
        }

        // Save to .env file for next time
        println!("💾 Saving keys to .env file...");
        let mut env_content = std::fs::read_to_string(".env").unwrap_or_default();
        if let Some(k) = &app_key
            && !env_content.contains("PLUQ_DROPBOX_APP_KEY=")
        {
            env_content.push_str(&format!("PLUQ_DROPBOX_APP_KEY=\"{k}\"\n"));
        }
        if let Some(s) = &app_secret
            && !env_content.contains("PLUQ_DROPBOX_APP_SECRET=")
        {
            env_content.push_str(&format!("PLUQ_DROPBOX_APP_SECRET=\"{s}\"\n"));
        }
        let _ = std::fs::write(".env", env_content);
    } else {
        println!("✅ API Credentials found.");
    }

    // 2. Optional Flags
    let use_debug = prompt_bool("🐛 Enable --debug logging? (y/n): ", true);
    let use_clean = prompt_bool("🧹 Enable --clean-cache? (y/n): ", true);

    // 3. Build and Run Command
    println!("\n🚀 Launching TUI Browse...");

    let mut args = vec!["run", "-p", "pluq", "--"];
    if use_debug {
        args.push("--debug");
    }
    if use_clean {
        args.push("--clean-cache");
    }
    args.push(&url);

    let mut cmd = Command::new("cargo");
    cmd.args(&args);

    // Set the env var for the child process
    if let Some(key) = app_key {
        cmd.env("PLUQ_DROPBOX_APP_KEY", key);
    }
    if let Some(secret) = app_secret {
        cmd.env("PLUQ_DROPBOX_APP_SECRET", secret);
    }

    // Inherit stdin/stdout/stderr so the TUI works correctly
    let status = cmd.status().expect("failed to execute pluq");

    if !status.success() {
        eprintln!("\n❌ TUI exited with error code: {}", status);
    }
}

// ── Google Drive Launcher (Old Code) ───────────────────────────────────────────

fn run_gdrive_launcher() {
    println!("=== 🎯 pluq: Google Drive Test Launcher ===\n");

    // 0. Load default URLs from dev_links.json
    let dev_links = match std::fs::read_to_string(".workspace/dev_links.json")
        .or_else(|_| std::fs::read_to_string("dev_links.json"))
    {
        Ok(s) => serde_json::from_str::<serde_json::Value>(&s).ok(),
        Err(_) => None,
    };

    let default_gdrive_url = dev_links
        .as_ref()
        .and_then(|v| v.get("default_gdrive_url"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://drive.google.com/");

    // 1. Check for API Key (Env -> .env file -> Prompt)
    let mut api_key = std::env::var("PLUQ_GDRIVE_KEY").ok();

    if api_key.is_none() {
        // Try reading from .env file
        if let Ok(content) = std::fs::read_to_string(".env") {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("PLUQ_GDRIVE_KEY=") {
                    api_key = Some(val.trim().trim_matches('\"').to_string());
                    break;
                }
            }
        }
    }

    if api_key.is_none() {
        println!("⚠️  PLUQ_GDRIVE_KEY is not set.");
        let k = prompt("🔑 Enter your Google Drive API Key: ");
        let k = k.trim().to_string();
        if k.is_empty() {
            println!("❌ Error: API key is required.");
            return;
        }

        // Save to .env file for next time
        println!("💾 Saving key to .env file...");
        let _ = std::fs::write(".env", format!("PLUQ_GDRIVE_KEY=\"{k}\"\n"));
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

    let mut args = vec!["run", "-p", "pluq", "--"];
    if use_debug {
        args.push("--debug");
    }
    if use_clean {
        args.push("--clean-cache");
    }
    args.push(&url);

    let mut cmd = Command::new("cargo");
    cmd.args(&args);

    // Set the env var for the child process
    if let Some(key) = api_key {
        cmd.env("PLUQ_GDRIVE_KEY", key);
    }

    // Inherit stdin/stdout/stderr so the TUI works correctly
    let status = cmd.status().expect("failed to execute pluq");

    if !status.success() {
        eprintln!("\n❌ TUI exited with error code: {}", status);
    }
}

// ── Dropbox PoC (New Code) ───────────────────────────────────────────────────

fn run_dropbox_poc() {
    println!("=== 🎯 pluq: Dropbox API PoC ===\n");

    let dev_links = match std::fs::read_to_string(".workspace/dev_links.json")
        .or_else(|_| std::fs::read_to_string("dev_links.json"))
    {
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

    if let Some(url) = personal_url {
        println!("1. Personal: {}", url);
    }
    if let Some(url) = public_url {
        println!("2. Public:   {}", url);
    }
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

    // Try to get credentials from env
    let mut app_key = std::env::var("PLUQ_DROPBOX_APP_KEY").ok();
    let mut app_secret = std::env::var("PLUQ_DROPBOX_APP_SECRET").ok();

    if (app_key.is_none() || app_secret.is_none())
        && let Ok(content) = std::fs::read_to_string(".env")
    {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("PLUQ_DROPBOX_APP_KEY=") {
                app_key = Some(val.trim().trim_matches('\"').to_string());
            }
            if let Some(val) = line.strip_prefix("PLUQ_DROPBOX_APP_SECRET=") {
                app_secret = Some(val.trim().trim_matches('\"').to_string());
            }
        }
    }

    if app_key.is_none() || app_secret.is_none() {
        println!("⚠️  Dropbox app credentials are not fully set.");
        if app_key.is_none() {
            let k = prompt("🔑 Enter your Dropbox App Key: ");
            app_key = Some(k.trim().to_string());
        }
        if app_secret.is_none() {
            let s = prompt("🔑 Enter your Dropbox App Secret: ");
            app_secret = Some(s.trim().to_string());
        }
    }

    let client = Client::new();

    // Step 1: Get Metadata (Size)
    println!("\n📊 1. Fetching Metadata...");
    let meta_url = "https://api.dropboxapi.com/2/sharing/get_shared_link_metadata";
    let mut meta_req = client
        .post(meta_url)
        .header("Content-Type", "application/json")
        .json(&json!({ "url": url }));

    if let (Some(k), Some(s)) = (app_key.as_ref(), app_secret.as_ref()) {
        meta_req = meta_req.basic_auth(k, Some(s));
    }

    let meta_resp = meta_req.send().expect("failed to send metadata request");
    println!("📡 Metadata Status: {}", meta_resp.status());

    let meta_body: serde_json::Value = meta_resp.json().expect("failed to parse metadata JSON");
    if meta_body.get("error").is_some() {
        println!("❌ Metadata Error: {}", meta_body);
        return;
    }

    let size = meta_body
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let name = meta_body
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    println!("✅ Name: {}, Size: {} bytes", name, size);

    // Step 2: Surgical Read (Range)
    println!("\n🎯 2. Performing Surgical Read (Range: bytes=0-19)...");
    let dl_url = "https://content.dropboxapi.com/2/sharing/get_shared_link_file";
    let mut dl_req = client
        .post(dl_url)
        .header("Dropbox-API-Arg", json!({ "url": url }).to_string())
        .header("Range", "bytes=0-19");

    if let (Some(k), Some(s)) = (app_key.as_ref(), app_secret.as_ref()) {
        dl_req = dl_req.basic_auth(k, Some(s));
    }

    let mut dl_resp = dl_req.send().expect("failed to send download request");
    println!("📡 Download Status: {}", dl_resp.status());

    if dl_resp.status().is_success() {
        let mut buffer = Vec::new();
        dl_resp
            .read_to_end(&mut buffer)
            .expect("failed to read body");
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

// ── Dropbox Range Warm-up Probe ───────────────────────────────────────────────
// Dirty empirical test: does a COLD tail-Range request to a Dropbox shared link
// return 200 (Range ignored) and later flip to 206 (Range honored)? Hammers the
// exact production request (get_shared_link_file + Range over the ZIP-EOCD tail)
// and prints status + content-range + elapsed. NEVER reads the body, so a 200
// full-file response does not download gigabytes. NOT production code.

struct ProbeResult {
    status: u16,
    http_version: String,
    took: f64,
    /// Every response header, sorted (never the body).
    headers: Vec<(String, String)>,
}

/// Look up a header value in a captured header list.
fn hval<'a>(headers: &'a [(String, String)], key: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("-")
}

fn run_dropbox_range_probe() {
    println!("=== 🎯 pluq: Dropbox Range Probe (full header dump) ===\n");
    println!("Hammers the EXACT production tail-Range request and dumps every response");
    println!("header, so we can see WHY the same request flip-flops 200/206. Tallies the");
    println!("split + diffs the header keys. The response body is never downloaded.\n");

    // 1. Pick URL (same dev_links.json the other launchers use).
    let dev_links = std::fs::read_to_string(".workspace/dev_links.json")
        .or_else(|_| std::fs::read_to_string("dev_links.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let personal = dev_links
        .as_ref()
        .and_then(|v| v.get("dropbox_zip"))
        .and_then(|v| v.as_str());
    let public = dev_links
        .as_ref()
        .and_then(|v| v.get("dropbox_public_zip"))
        .and_then(|v| v.as_str());
    if let Some(u) = personal {
        println!("1. Personal: {u}");
    }
    if let Some(u) = public {
        println!("2. Public:   {u}");
    }
    println!("3. Custom URL");
    let url = match prompt("Select a link option (1/2/3): ").trim() {
        "1" => personal.map(String::from).unwrap_or_default(),
        "2" => public.map(String::from).unwrap_or_default(),
        _ => prompt("🔗 Enter Dropbox Shared Link: ").trim().to_string(),
    };
    if url.is_empty() {
        println!("❌ URL required.");
        return;
    }

    // 2. Credentials (env → .env).
    let (key, secret) = match dropbox_creds() {
        Some(c) => c,
        None => {
            println!("❌ Need PLUQ_DROPBOX_APP_KEY + PLUQ_DROPBOX_APP_SECRET (env or .env).");
            return;
        }
    };

    let attempts: u32 = prompt("Attempts (default 16): ")
        .trim()
        .parse()
        .unwrap_or(16);
    let interval: u64 = prompt("Interval seconds (default 2): ")
        .trim()
        .parse()
        .unwrap_or(2);

    let client = Client::builder()
        .user_agent("pluq-dev-probe")
        .timeout(Duration::from_secs(45))
        .build()
        .expect("client build");

    // 3. Metadata → size (so we know the tail offset). This DOES read the small JSON body.
    let meta: serde_json::Value = client
        .post("https://api.dropboxapi.com/2/sharing/get_shared_link_metadata")
        .basic_auth(&key, Some(&secret))
        .json(&json!({ "url": url }))
        .send()
        .expect("metadata send")
        .json()
        .expect("metadata json");
    let size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    if size == 0 {
        println!("❌ metadata error / not a file: {meta}");
        return;
    }
    let name = meta.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let tag = meta.get(".tag").and_then(|v| v.as_str()).unwrap_or("?");
    println!("\n✅ {name}  size={size}  .tag={tag}");

    const LEN: u64 = 65557; // identical to the ZIP-EOCD tail read in production
    let tail_off = size.saturating_sub(LEN);

    // One range probe of the EXACT production request. Reads status + headers only.
    let probe = |off: u64| -> ProbeResult {
        let range_end = off + LEN - 1;
        let t0 = Instant::now();
        let res = client
            .post("https://content.dropboxapi.com/2/sharing/get_shared_link_file")
            .basic_auth(&key, Some(&secret))
            .header("Dropbox-API-Arg", json!({ "url": url }).to_string())
            .header("Range", format!("bytes={off}-{range_end}"))
            .send();
        let took = t0.elapsed().as_secs_f64();
        match res {
            Ok(r) => {
                let mut headers: Vec<(String, String)> = r
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            v.to_str().unwrap_or("<binary>").to_string(),
                        )
                    })
                    .collect();
                headers.sort();
                let out = ProbeResult {
                    status: r.status().as_u16(),
                    http_version: format!("{:?}", r.version()),
                    took,
                    headers,
                };
                drop(r); // never read the body — a 200 here is the whole file
                out
            }
            Err(e) => ProbeResult {
                status: 0,
                http_version: format!("SEND-ERR: {e}"),
                took,
                headers: Vec::new(),
            },
        }
    };

    // How many edge IPs does the content host resolve to? (multiple = load-balanced)
    match ("content.dropboxapi.com", 443).to_socket_addrs() {
        Ok(addrs) => {
            let ips: BTreeSet<String> = addrs.map(|a| a.ip().to_string()).collect();
            println!(
                "\ncontent.dropboxapi.com → {} edge IP(s): {ips:?}",
                ips.len()
            );
        }
        Err(e) => println!("\n(could not resolve content.dropboxapi.com: {e})"),
    }

    // Baseline: the HEAD range, full headers.
    let h = probe(0);
    println!(
        "\n── HEAD off=0  status={} {} took={:.2}s ──",
        h.status, h.http_version, h.took
    );
    for (k, v) in &h.headers {
        println!("        {k}: {v}");
    }

    println!("\nHammering TAIL (off={tail_off}) {attempts}× every {interval}s — FULL HEADERS:\n");

    let start = Instant::now();
    let (mut n200, mut n206, mut nother) = (0u32, 0u32, 0u32);
    let mut keys_200: BTreeSet<String> = BTreeSet::new();
    let mut keys_206: BTreeSet<String> = BTreeSet::new();
    for i in 1..=attempts {
        let p = probe(tail_off);
        let t = start.elapsed().as_secs_f64();
        let marker = match p.status {
            206 => "✅206",
            200 => "🟡200",
            _ => "❓",
        };
        println!(
            "── [t+{t:>6.1}s] #{i:<2} {marker} status={} {} took={:.2}s  content-range={}  rid={} ──",
            p.status,
            p.http_version,
            p.took,
            hval(&p.headers, "content-range"),
            hval(&p.headers, "x-dropbox-request-id")
        );
        for (k, v) in &p.headers {
            println!("        {k}: {v}");
        }
        println!();
        match p.status {
            206 => {
                n206 += 1;
                keys_206.extend(p.headers.iter().map(|(k, _)| k.clone()));
            }
            200 => {
                n200 += 1;
                keys_200.extend(p.headers.iter().map(|(k, _)| k.clone()));
            }
            _ => nother += 1,
        }
        if i < attempts {
            std::thread::sleep(Duration::from_secs(interval));
        }
    }

    println!("=== TALLY ===");
    println!("206 (Range honored): {n206}");
    println!("200 (Range ignored): {n200}");
    if nother > 0 {
        println!("other / send-error : {nother}");
    }
    let total = n200 + n206 + nother;
    if total > 0 {
        println!(
            "→ {:.0}% of identical requests ignored Range (200).",
            100.0 * f64::from(n200) / f64::from(total)
        );
    }

    let only_206: Vec<String> = keys_206.difference(&keys_200).cloned().collect();
    let only_200: Vec<String> = keys_200.difference(&keys_206).cloned().collect();
    println!("\n=== HEADER-KEY DIFF (the tell) ===");
    println!("present ONLY on 206 responses: {only_206:?}");
    println!("present ONLY on 200 responses: {only_200:?}");
    println!(
        "(eyeball values above — server / via / x-dropbox-* / age / x-cache / etag —\n to see whether 200 vs 206 splits by backend, cache state, or HTTP version)"
    );
}

/// Dropbox app creds from env, falling back to `.env`. Dev-only.
fn dropbox_creds() -> Option<(String, String)> {
    let mut key = std::env::var("PLUQ_DROPBOX_APP_KEY").ok();
    let mut secret = std::env::var("PLUQ_DROPBOX_APP_SECRET").ok();
    if (key.is_none() || secret.is_none())
        && let Ok(content) = std::fs::read_to_string(".env")
    {
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("PLUQ_DROPBOX_APP_KEY=") {
                key = Some(v.trim().trim_matches('"').to_string());
            }
            if let Some(v) = line.strip_prefix("PLUQ_DROPBOX_APP_SECRET=") {
                secret = Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    match (key, secret) {
        (Some(k), Some(s)) if !k.is_empty() && !s.is_empty() => Some((k, s)),
        _ => None,
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
