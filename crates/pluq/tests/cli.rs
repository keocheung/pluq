use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// =========================================================================
// list subcommand
// =========================================================================

#[test]
fn list_zip() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg(fixture_path("basic.zip"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("readme.txt"))
        .stdout(predicates::str::contains("big.txt"))
        .stdout(predicates::str::contains("photos/2024/pic.txt"))
        .stdout(predicates::str::contains(
            "2 OS junk files hidden — use --all to show",
        ))
        .stdout(predicates::str::contains(".DS_Store").not());
}

#[test]
fn list_all_shows_junk() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg("--all").arg(fixture_path("basic.zip"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains(".DS_Store"))
        .stdout(predicates::str::contains("__MACOSX/._readme.txt"))
        .stdout(predicates::str::contains("8 files total"));
}

#[test]
fn list_rar5() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg(fixture_path("basic_rar5.rar"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("readme.txt"))
        .stdout(predicates::str::contains("big.txt"));
}

#[test]
fn list_rar4() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg(fixture_path("legacy_rar4.rar"));
    cmd.assert().success();
}

#[test]
fn list_empty() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg(fixture_path("empty.zip"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("0 files total"));
}

// =========================================================================
// get subcommand
// =========================================================================

#[test]
fn get_to_output_dir() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("basic.zip"))
        .arg("-o")
        .arg(tmp.path());
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("file(s) extracted"));

    assert!(tmp.path().join("readme.txt").exists());
    let readme_content = std::fs::read_to_string(tmp.path().join("readme.txt")).unwrap();
    assert_eq!(readme_content.trim(), "hello world");

    // Check unicode file (handles either decoded/unescaped or raw representations depending on platform/git)
    let unicode_exists = tmp.path().join("файл-日本語-€.txt").exists()
        || tmp.path().join("Ñ„Ð°Ð¹Ð»-æ—¥æœ¬èªž-â‚¬.txt").exists();
    assert!(unicode_exists);
}

#[test]
fn get_default_dir() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.current_dir(tmp.path())
        .arg("get")
        .arg(fixture_path("basic.zip"));
    cmd.assert().success();

    // Suggested name is basic.zip -> basic/
    assert!(tmp.path().join("basic").join("readme.txt").exists());
}

#[test]
fn get_basename_glob() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("basic.zip"))
        .arg("*.txt")
        .arg("-o")
        .arg(tmp.path());
    cmd.assert().success();

    assert!(tmp.path().join("readme.txt").exists());
    assert!(!tmp.path().join("random_store.bin").exists());
}

#[test]
fn get_path_glob() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("basic.zip"))
        .arg("photos/**/*.txt")
        .arg("-o")
        .arg(tmp.path());
    cmd.assert().success();

    assert!(tmp.path().join("photos/2024/pic.txt").exists());
    assert!(!tmp.path().join("readme.txt").exists());
}

#[test]
fn get_glob_no_match() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("basic.zip"))
        .arg("nomatch*")
        .arg("-o")
        .arg(tmp.path());
    cmd.assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains(
            "No entries match glob 'nomatch*'",
        ));
}

#[test]
fn get_output_dir_with_spaces() {
    let tmp = tempdir().unwrap();
    let out_dir = tmp.path().join("out dir");
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("basic.zip"))
        .arg("-o")
        .arg(&out_dir);
    cmd.assert().success();

    assert!(out_dir.join("readme.txt").exists());
}

#[test]
fn get_empty() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("empty.zip"))
        .arg("-o")
        .arg(tmp.path());
    cmd.assert().success().stderr(predicates::str::contains(
        "Archive contains no extractable entries",
    ));
}

// =========================================================================
// test subcommand
// =========================================================================

#[test]
fn test_zip_ok() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("test").arg(fixture_path("basic.zip"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("All 6 file(s) OK"));
}

#[test]
fn test_rar5_ok() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("test").arg(fixture_path("basic_rar5.rar"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("file(s) OK"));
}

#[test]
fn test_corrupt_fails() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("test").arg(fixture_path("corrupt.zip"));
    cmd.assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("FAIL"))
        .stderr(predicates::str::contains("file(s) FAILED"));
}

// =========================================================================
// symlinks (Edge Cases)
// =========================================================================

#[cfg(unix)]
#[test]
fn get_symlink_safe() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("symlink_safe.zip"))
        .arg("-o")
        .arg(tmp.path());
    cmd.assert().success();

    assert!(tmp.path().join("target.txt").exists());
    assert!(tmp.path().join("link.txt").exists());
}

#[cfg(unix)]
#[test]
fn get_symlink_escape_blocked() {
    let probe = Path::new("/tmp/pluq_escape_probe");
    if probe.exists() {
        std::fs::remove_file(probe).ok();
    }

    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("symlink_escape.zip"))
        .arg("-o")
        .arg(tmp.path());

    cmd.assert()
        .success() // Partial success exits with 0
        .stderr(predicates::str::contains("escape.txt"))
        .stderr(predicates::str::contains("file(s) failed:"));

    assert!(!probe.exists());
}

// =========================================================================
// solid RAR (Edge Cases)
// =========================================================================

#[test]
fn solid_rar_partial() {
    let tmp = tempdir().unwrap();
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("get")
        .arg(fixture_path("solid_rar5.rar"))
        .arg("*.txt")
        .arg("-o")
        .arg(tmp.path());

    // Should complete, not panic, and exit cleanly
    let assert_res = cmd.assert();
    let output = assert_res.get_output();
    let status = output.status.code();

    // Output status must be Some(0) or Some(1), not panic/signal/101
    assert!(status == Some(0) || status == Some(1));
}

// =========================================================================
// errors / misc
// =========================================================================

#[test]
fn nonexistent_file() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg("/nonexistent_xyz.zip");
    cmd.assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("Error:"));
}

#[test]
fn non_archive_file() {
    let tmp = tempdir().unwrap();
    let txt_file = tmp.path().join("not_archive.txt");
    std::fs::write(&txt_file, "plain text not zip").unwrap();

    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("list").arg(&txt_file);
    cmd.assert()
        .failure()
        .code(1)
        .stderr(predicates::str::contains("Error:"))
        .stderr(predicates::str::contains("request support"));
}

#[test]
fn completions_zsh() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("completions").arg("zsh");
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("#compdef"))
        .stdout(predicates::str::contains("_pluq"));
}

#[test]
fn clean_cache_smoke() {
    let mut cmd = Command::cargo_bin("pluq").unwrap();
    cmd.arg("--clean-cache")
        .arg("list")
        .arg(fixture_path("basic.zip"));
    cmd.assert()
        .success()
        .stdout(predicates::str::contains("readme.txt"));
}
