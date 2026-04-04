use archive_core::LocalPath;
use archive_source::{resolve, ResolvedSource, SourceConfig};

#[test]
fn resolve_local_path() {
    let config = SourceConfig::new();
    let result = resolve("/tmp/test.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Local(_)));
}

#[test]
fn resolve_local_path_relative() {
    let config = SourceConfig::new();
    let result = resolve("./test.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Local(_)));
}

#[test]
fn resolve_http_url() {
    let config = SourceConfig::new();
    let result = resolve("https://example.com/file.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Http(_)));
}

#[test]
fn resolve_http_url_no_tls() {
    let config = SourceConfig::new();
    let result = resolve("http://example.com/file.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Http(_)));
}

#[test]
fn resolve_gdrive_url_without_api_key() {
    // Without ARCHIVE_SNIPER_GDRIVE_KEY: Err(NoCredentials).
    // With key set: Ok(GDrive(_)). Both are valid outcomes.
    let config = SourceConfig::new();
    let result = resolve(
        "https://drive.google.com/file/d/abc123/view?usp=sharing",
        &config,
    );
    assert!(matches!(result, Err(_) | Ok(ResolvedSource::GDrive(_))));
}

#[test]
fn resolve_gdrive_url_no_file_id() {
    // drive.google.com URL with no extractable file ID.
    // No API key: Err(NoCredentials). Key set: Err(InvalidUrl). Always Err.
    let config = SourceConfig::new();
    let result = resolve("https://drive.google.com/drive/my-drive", &config);
    assert!(result.is_err());
}

#[test]
fn local_path_returns_some_for_local() {
    let config = SourceConfig::new();
    let source = resolve("/tmp/test.rar", &config).unwrap();
    assert!(source.local_path().is_some());
}

#[test]
fn local_path_returns_none_for_http() {
    let config = SourceConfig::new();
    let source = resolve("https://example.com/test.rar", &config).unwrap();
    assert!(source.local_path().is_none());
}
