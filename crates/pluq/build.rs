use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Target triple — only available to build scripts.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=PLUQ_BUILD_TARGET={target}");

    // Short git SHA. "unknown" when built outside a git checkout (release tarball).
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=PLUQ_BUILD_GIT_SHA={sha}");

    // Build timestamp (unix seconds). Formatted at runtime with chrono.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=PLUQ_BUILD_TIMESTAMP={ts}");

    // Re-run when HEAD moves so the SHA stays fresh between builds.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
