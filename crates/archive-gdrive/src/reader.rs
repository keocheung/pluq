use archive_core::io::{HttpReadAt, ReadAt, ReadError};
use bytes::Bytes;
use reqwest::Client;
use tokio::io::AsyncRead;
use tokio::sync::OnceCell;
use tracing::{debug, info};

use crate::error::GDriveError;

pub struct GoogleDriveReadAt {
    /// The inner reader pointed at the googleapis download URL.
    inner: HttpReadAt,
    /// Google Drive file ID (for metadata lookups and error messages).
    file_id: String,
    /// API key.
    api_key: String,
    /// HTTP client (shared with inner reader).
    client: Client,
    /// Base URL for API requests. Production: "https://www.googleapis.com".
    api_base_url: String,
    /// Lazily populated file metadata.
    metadata: OnceCell<GDriveMetadata>,
}

struct GDriveMetadata {
    #[allow(dead_code)] // stored for future display/error context
    name: String,
    size: u64,
}

impl GoogleDriveReadAt {
    pub fn new(file_id: String, api_key: String) -> Self {
        Self::build(file_id, api_key, "https://www.googleapis.com")
    }

    fn build(file_id: String, api_key: String, base_url: &str) -> Self {
        let download_url = format!(
            "{base_url}/drive/v3/files/{}?alt=media&key={}",
            file_id, api_key
        );
        let client = Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        let inner = HttpReadAt::new(&download_url, client.clone());
        Self {
            inner,
            file_id,
            api_key,
            client,
            api_base_url: base_url.to_string(),
            metadata: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_base_url(file_id: String, api_key: String, base_url: &str) -> Self {
        Self::build(file_id, api_key, base_url)
    }

    async fn ensure_metadata(&self) -> Result<&GDriveMetadata, ReadError> {
        self.metadata
            .get_or_try_init(|| async {
                let url = format!(
                    "{}/drive/v3/files/{}?key={}&fields=name,size,mimeType",
                    self.api_base_url, self.file_id, self.api_key
                );

                debug!(file_id = %self.file_id, "fetching Google Drive metadata");
                let resp = self.client.get(&url).send().await.map_err(|e| {
                    transport_error(GDriveError::Api {
                        status: 0,
                        detail: format!("metadata request failed: {e}"),
                    })
                })?;

                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(transport_error(classify_api_error(
                        status.as_u16(),
                        &body,
                        &self.file_id,
                    )));
                }

                let json: serde_json::Value = resp.json().await.map_err(|e| {
                    transport_error(GDriveError::Api {
                        status: 200,
                        detail: format!("invalid metadata JSON: {e}"),
                    })
                })?;

                let name = json["name"].as_str().unwrap_or("unknown").to_string();
                // Google Drive returns size as a string, not a number.
                let size = json["size"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);

                info!(file_id = %self.file_id, name = %name, size, "Google Drive metadata OK");
                Ok(GDriveMetadata { name, size })
            })
            .await
    }
}

/// Wrap a `GDriveError` in `ReadError::Transport` for tunneling through the layer stack.
fn transport_error(e: GDriveError) -> ReadError {
    ReadError::Transport(Box::new(e))
}

/// Classify a Google Drive API error response into a specific `GDriveError`.
fn classify_api_error(status: u16, body: &str, file_id: &str) -> GDriveError {
    let reason = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|j| {
            j["error"]["errors"][0]["reason"]
                .as_str()
                .map(|s| s.to_string())
        });

    match status {
        404 => GDriveError::FileNotFound {
            file_id: file_id.to_string(),
        },
        403 => match reason.as_deref() {
            Some("rateLimitExceeded") => GDriveError::RateLimited {
                retry_after_secs: 60,
            },
            Some("userRateLimitExceeded") => GDriveError::RateLimited {
                retry_after_secs: 30,
            },
            Some("dailyLimitExceeded") => GDriveError::QuotaExceeded,
            _ => GDriveError::NotPublic {
                file_id: file_id.to_string(),
            },
        },
        401 => GDriveError::InvalidApiKey,
        429 => GDriveError::RateLimited {
            retry_after_secs: 60,
        },
        _ => GDriveError::Api {
            status,
            detail: body.to_string(),
        },
    }
}

impl ReadAt for GoogleDriveReadAt {
    async fn size(&self) -> Result<u64, ReadError> {
        let meta = self.ensure_metadata().await?;
        Ok(meta.size)
    }

    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        self.inner.read_range(offset, len).await
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        self.inner.read_stream(offset, len).await
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    use super::*;

    #[tokio::test]
    async fn size_returns_metadata_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "test.rar",
                "size": "1000",
                "mimeType": "application/x-rar-compressed"
            })))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "test_file_id".to_string(),
            "test_key".to_string(),
            &server.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 1000);
    }

    #[tokio::test]
    async fn read_range_delegates_to_inner() {
        let server = MockServer::start().await;
        // Only match requests that include a Range header (the actual data request).
        Mock::given(method("GET"))
            .and(header("range", "bytes=0-9"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"0123456789"))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "test_file_id".to_string(),
            "test_key".to_string(),
            &server.uri(),
        );
        let bytes = reader.read_range(0, 10).await.unwrap();
        assert_eq!(bytes.as_ref(), b"0123456789");
    }

    #[tokio::test]
    async fn not_found_tunnels_as_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": 404,
                    "message": "File not found.",
                    "errors": [{"reason": "notFound"}]
                }
            })))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "missing_id".to_string(),
            "test_key".to_string(),
            &server.uri(),
        );
        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<GDriveError>(),
                Some(GDriveError::FileNotFound { .. })
            ),
            "expected FileNotFound, got: {:?}",
            e.downcast_ref::<GDriveError>()
        );
    }

    #[tokio::test]
    async fn not_public_tunnels_as_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "Insufficient permissions.",
                    "errors": [{"reason": "insufficientPermissions"}]
                }
            })))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "private_id".to_string(),
            "test_key".to_string(),
            &server.uri(),
        );
        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<GDriveError>(),
                Some(GDriveError::NotPublic { .. })
            ),
            "expected NotPublic, got: {:?}",
            e.downcast_ref::<GDriveError>()
        );
    }

    #[tokio::test]
    async fn invalid_api_key_tunnels() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {
                    "code": 401,
                    "message": "API key invalid.",
                    "errors": [{"reason": "authError"}]
                }
            })))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "test_id".to_string(),
            "bad_key".to_string(),
            &server.uri(),
        );
        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<GDriveError>(),
                Some(GDriveError::InvalidApiKey)
            ),
            "expected InvalidApiKey, got: {:?}",
            e.downcast_ref::<GDriveError>()
        );
    }
}
