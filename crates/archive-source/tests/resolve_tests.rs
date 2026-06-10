use archive_core::LocalPath;
use archive_source::{ResolvedSource, SourceConfig, resolve};

#[test]
fn resolve_local_path() {
    let config = SourceConfig::new();
    let result = resolve("/tmp/test.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Local(_, _)));
}

#[test]
fn resolve_local_path_relative() {
    let config = SourceConfig::new();
    let result = resolve("./test.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Local(_, _)));
}

#[test]
fn resolve_http_url() {
    let config = SourceConfig::new();
    let result = resolve("https://example.com/file.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Http(_, _)));
}

#[test]
fn resolve_http_url_no_tls() {
    let config = SourceConfig::new();
    let result = resolve("http://example.com/file.zip", &config).unwrap();
    assert!(matches!(result, ResolvedSource::Http(_, _)));
}

#[test]
fn resolve_gdrive_url_without_api_key() {
    let config = SourceConfig::new();
    let result = resolve(
        "https://drive.google.com/file/d/abc123/view?usp=sharing",
        &config,
    );
    assert!(matches!(
        result,
        Err(_) | Ok(ResolvedSource::GDrive(_, _, _))
    ));
}

#[test]
fn resolve_gdrive_url_no_file_id() {
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

#[tokio::test]
async fn test_resolved_name() {
    let config = SourceConfig::new();

    // Local path (pure, no network)
    let res = resolve("my_archive.zip", &config).unwrap();
    assert_eq!(res.resolved_name().await.unwrap(), "my_archive");

    // HTTP URL (pure, no network)
    let res = resolve("https://example.com/data.rar?v=1", &config).unwrap();
    assert_eq!(res.resolved_name().await.unwrap(), "data");
}

// A Google Drive "view" link with no fetched metadata falls back to the file
// ID via pure URL parsing — exercised here without touching the network.
#[test]
fn test_gdrive_name_fallback_is_file_id() {
    assert_eq!(
        archive_source::suggest_name_from_str("https://drive.google.com/file/d/ABC-123/view")
            .unwrap(),
        "ABC-123"
    );
}
