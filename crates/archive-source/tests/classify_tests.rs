use std::time::Duration;

use archive_gdrive::GDriveError;
use archive_source::{ProviderError, classify_transport};

#[test]
fn classify_gdrive_file_not_found() {
    let err = GDriveError::FileNotFound {
        file_id: "abc123".to_string(),
    };
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::FileNotFound {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_not_public() {
    let err = GDriveError::NotPublic {
        file_id: "abc123".to_string(),
    };
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::NotPublic {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_invalid_api_key() {
    let err = GDriveError::InvalidApiKey;
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::InvalidCredentials {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_api_key_missing() {
    let err = GDriveError::ApiKeyMissing;
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::InvalidCredentials {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_rate_limited() {
    let err = GDriveError::RateLimited {
        retry_after_secs: 30,
    };
    let result = classify_transport(&err).unwrap();
    match result {
        ProviderError::RateLimited {
            provider: "Google Drive",
            retry_after: Some(d),
        } => {
            assert_eq!(d, Duration::from_secs(30));
        }
        _ => panic!("expected RateLimited {{ provider: Google Drive, retry_after: Some(30s) }}"),
    }
}

#[test]
fn classify_gdrive_quota_exceeded() {
    let err = GDriveError::QuotaExceeded;
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::QuotaExceeded {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_invalid_url() {
    let err = GDriveError::InvalidUrl {
        url: "https://drive.google.com/".to_string(),
    };
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::InvalidCredentials {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_gdrive_api_error() {
    let err = GDriveError::Api {
        status: 500,
        detail: "internal server error".to_string(),
    };
    let result = classify_transport(&err);
    assert!(matches!(
        result,
        Some(ProviderError::InvalidCredentials {
            provider: "Google Drive"
        })
    ));
}

#[test]
fn classify_unknown_error_returns_none() {
    let err = std::io::Error::other("random error");
    let result = classify_transport(&err);
    assert!(result.is_none());
}
