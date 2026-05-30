use archive::ArchiveConfig;
use archive_cache::{open, Opened};
use archive_source::SourceConfig;

fn dev_link(key: &str) -> Option<String> {
    let env_key = format!("PLUQ_{}", key.to_uppercase());
    if let Ok(val) = std::env::var(&env_key) {
        return Some(val);
    }
    let paths = [
        ".workspace/dev_links.json",
        "../../.workspace/dev_links.json",
        "dev_links.json",
        "../../dev_links.json",
    ];
    for path in paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(val) = v.get(key).and_then(|v| v.as_str()) {
            return Some(val.to_string());
        }
    }
    None
}

const GH_URL: &str = "https://raw.githubusercontent.com/ssokolow/rar-test-files/16b785c2b1b504e99fc307676e5369a26d3ce060/build/testfile.rar5.rar";

#[tokio::test]
#[ignore = "requires network"]
async fn test_http_metadata_caching() {
    let source_config = SourceConfig::default();
    let archive_config = ArchiveConfig::default();

    if let Some(p) = archive_cache::cache_path_for(GH_URL) {
        let _ = std::fs::remove_file(p);
    }

    let opened = open(GH_URL, &source_config, archive_config.clone())
        .await
        .unwrap();

    let _archive = match opened {
        Opened::NeedsScan(p) => p
            .scan(std::sync::Arc::new(std::sync::Mutex::new(())))
            .await
            .unwrap(),
        Opened::Ready(_) => panic!("Should need scan on first open"),
    };

    let cache_path = archive_cache::cache_path_for(GH_URL).unwrap();
    assert!(cache_path.exists(), "Cache file should be created");

    let cached = archive_cache::ArchiveCache::load(&cache_path).unwrap();
    assert_eq!(cached.fingerprint.source, GH_URL);
    assert!(cached.fingerprint.source_size > 0);
    assert!(
        cached.fingerprint.etag.is_some() || cached.fingerprint.last_modified.is_some(),
        "HTTP source should capture ETag or Last-Modified"
    );

    let opened2 = open(GH_URL, &source_config, archive_config).await.unwrap();
    assert!(
        matches!(opened2, Opened::Ready(_)),
        "Second open should hit cache"
    );
}

#[tokio::test]
#[ignore = "requires network"]
async fn test_dropbox_metadata_caching() {
    let url = match dev_link("dropbox_zip") {
        Some(u) => u,
        None => return,
    };

    let source_config = SourceConfig::default();
    let archive_config = ArchiveConfig::default();

    if let Some(p) = archive_cache::cache_path_for(&url) {
        let _ = std::fs::remove_file(p);
    }

    let opened = open(&url, &source_config, archive_config.clone())
        .await
        .unwrap();

    let _archive = match opened {
        Opened::NeedsScan(p) => p
            .scan(std::sync::Arc::new(std::sync::Mutex::new(())))
            .await
            .unwrap(),
        Opened::Ready(_) => panic!("Should need scan on first open"),
    };

    let cache_path = archive_cache::cache_path_for(&url).unwrap();
    assert!(cache_path.exists());

    let cached = archive_cache::ArchiveCache::load(&cache_path).unwrap();
    assert!(
        cached.fingerprint.etag.is_some() || cached.fingerprint.last_modified.is_some(),
        "Dropbox should return ETag or Last-Modified"
    );

    let opened2 = open(&url, &source_config, archive_config).await.unwrap();
    assert!(matches!(opened2, Opened::Ready(_)));
}

#[tokio::test]
#[ignore = "requires network"]
async fn test_gdrive_metadata_caching() {
    let url = match dev_link("gdrive_rar") {
        Some(u) => u,
        None => return,
    };

    let source_config = SourceConfig::default();
    let archive_config = ArchiveConfig::default();

    if let Some(p) = archive_cache::cache_path_for(&url) {
        let _ = std::fs::remove_file(p);
    }

    let opened = match open(&url, &source_config, archive_config.clone()).await {
        Ok(o) => o,
        Err(_) => return, // Google drive might fail without auth
    };

    let _archive = match opened {
        Opened::NeedsScan(p) => {
            match p.scan(std::sync::Arc::new(std::sync::Mutex::new(()))).await {
                Ok(a) => a,
                Err(_) => return, // Might fail if it's an HTML page
            }
        }
        Opened::Ready(_) => panic!("Should need scan on first open"),
    };

    let cache_path = archive_cache::cache_path_for(&url).unwrap();
    assert!(cache_path.exists());

    let opened2 = open(&url, &source_config, archive_config).await.unwrap();
    assert!(matches!(opened2, Opened::Ready(_)));
}
