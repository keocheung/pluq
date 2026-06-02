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

    link_compiler_rt_builtins(&target);
}

/// macOS x86_64: UnRAR's SSE/AES-NI dispatch (`system.cpp`, `rijndael.cpp`) calls
/// `__builtin_cpu_supports`, which clang lowers to `__cpu_model` /
/// `__cpu_indicator_init`. Those live in compiler-rt's `libclang_rt.osx.a`, and
/// Rust's link step doesn't pull that builtins archive in for the bundled C++
/// static lib — so they come up as undefined symbols at link time. Linux x86_64
/// gets them from libgcc by default; ARM macOS has no `__cpu_model`. Link the
/// builtins archive explicitly for this one target. Link args from the binary
/// crate apply to the final link, so the fix belongs here, not in `unrar_sys`.
fn link_compiler_rt_builtins(target: &str) {
    if target != "x86_64-apple-darwin" {
        return;
    }
    let resource_dir = Command::new("clang")
        .arg("-print-resource-dir")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());
    if let Some(dir) = resource_dir {
        let lib_dir = std::path::Path::new(dir.trim()).join("lib/darwin");
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    }
}
