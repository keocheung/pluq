use archive_core::LocalPath;
use archive_source::{resolve, ResolvedSource, SourceConfig};

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
    assert!(matches!(result, Err(_) | Ok(ResolvedSource::GDrive(_, _, _))));
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

#[test]
fn test_suggested_name() {
    let config = SourceConfig::new();

    // Local path
    let res = resolve("my_archive.zip", &config).unwrap();
    assert_eq!(res.suggested_name().unwrap(), "my_archive");

    // HTTP URL
    let res = resolve("https://example.com/data.rar?v=1", &config).unwrap();
    assert_eq!(res.suggested_name().unwrap(), "data");

    // GDrive "view" link (mocking key to test logic)
    unsafe { std::env::set_var("ARCHIVE_SNIPER_GDRIVE_KEY", "dummy"); }
    let res = resolve("https://drive.google.com/file/d/ABC-123/view", &config).unwrap();
    assert_eq!(res.suggested_name().unwrap(), "ABC-123");
}
