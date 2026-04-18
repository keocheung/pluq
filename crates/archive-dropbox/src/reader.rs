use archive_core::io::{ReadAt, ReadError};
use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::Client;
use tokio::io::AsyncRead;
use tokio::sync::OnceCell;
use tracing::{debug, error, info, trace};

use crate::error::DropboxError;

pub struct DropboxReadAt {
    shared_url: String,
    app_key: String,
    app_secret: String,
    client: Client,
    api_base_url: String,     // default: https://api.dropboxapi.com
    content_base_url: String, // default: https://content.dropboxapi.com
    metadata: OnceCell<DropboxMetadata>,
}

struct DropboxMetadata {
    #[allow(dead_code)]
    name: String,
    size: u64,
}

impl DropboxReadAt {
    pub fn new(shared_url: String, app_key: String, app_secret: String) -> Self {
        Self::build(
            shared_url,
            app_key,
            app_secret,
            "https://api.dropboxapi.com",
            "https://content.dropboxapi.com",
        )
    }

    fn build(
        shared_url: String,
        app_key: String,
        app_secret: String,
        api_base: &str,
        content_base: &str,
    ) -> Self {
        let client = Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        Self {
            shared_url,
            app_key,
            app_secret,
            client,
            api_base_url: api_base.to_string(),
            content_base_url: content_base.to_string(),
            metadata: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_base_urls(
        shared_url: String,
        app_key: String,
        app_secret: String,
        api_base: &str,
        content_base: &str,
    ) -> Self {
        Self::build(shared_url, app_key, app_secret, api_base, content_base)
    }

    async fn ensure_metadata(&self) -> Result<&DropboxMetadata, ReadError> {
        self.metadata
            .get_or_try_init(|| async {
                debug!(url = %self.shared_url, "Fetching Dropbox shared link metadata");
                let url = format!("{}/2/sharing/get_shared_link_metadata", self.api_base_url);
                let body = serde_json::json!({ "url": self.shared_url });

                let resp = self
                    .client
                    .post(&url)
                    .basic_auth(&self.app_key, Some(&self.app_secret))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| {
                        error!(error = %e, "Dropbox metadata request failed");
                        transport_error(DropboxError::Api {
                            status: 0,
                            detail: format!("metadata request failed: {e}"),
                        })
                    })?;

                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();

                if !status.is_success() {
                    let err = classify_api_error(status.as_u16(), &text, &self.shared_url);
                    error!(status = status.as_u16(), error = ?err, "Dropbox metadata API error");
                    return Err(transport_error(err));
                }

                let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                    error!(error = %e, "Invalid Dropbox metadata JSON response");
                    transport_error(DropboxError::Api {
                        status: 200,
                        detail: format!("invalid metadata JSON: {e}"),
                    })
                })?;

                let size = json["size"].as_u64().unwrap_or(0);
                let name = json["name"].as_str().unwrap_or("unknown").to_string();

                if json[".tag"].as_str() != Some("file") {
                    error!(tag = ?json[".tag"], "Dropbox shared link is not a file");
                    return Err(transport_error(DropboxError::InvalidUrl {
                        url: self.shared_url.clone(),
                    }));
                }

                info!(name = %name, size = size, "Dropbox metadata resolved");
                Ok(DropboxMetadata { name, size })
            })
            .await
    }

    async fn send_content_request(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<reqwest::Response, ReadError> {
        trace!(
            offset = offset,
            len = len,
            "Requesting Dropbox file content range"
        );
        let url = format!("{}/2/sharing/get_shared_link_file", self.content_base_url);
        let range_end = offset + len - 1;
        let api_arg = serde_json::json!({ "url": self.shared_url }).to_string();

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.app_key, Some(&self.app_secret))
            .header("Dropbox-API-Arg", api_arg)
            .header("Range", format!("bytes={offset}-{range_end}"))
            .send()
            .await
            .map_err(|e| {
                error!(error = %e, "Dropbox content request failed");
                transport_error(DropboxError::Api {
                    status: 0,
                    detail: format!("content request failed: {e}"),
                })
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            return Ok(resp);
        }
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            error!(offset = offset, len = len, "Dropbox range not satisfiable");
            return Err(ReadError::OutOfBounds {
                offset,
                len,
                size: 0,
            });
        }
        let body = resp.text().await.unwrap_or_default();
        let err = classify_api_error(status.as_u16(), &body, &self.shared_url);
        error!(status = status.as_u16(), error = ?err, "Dropbox content API error");
        Err(transport_error(err))
    }
}

impl ReadAt for DropboxReadAt {
    async fn size(&self) -> Result<u64, ReadError> {
        Ok(self.ensure_metadata().await?.size)
    }

    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let resp = self.send_content_request(offset, len).await?;
        resp.bytes().await.map_err(|e| {
            error!(error = %e, "Failed to read Dropbox response body");
            transport_error(DropboxError::Api {
                status: 0,
                detail: format!("body read failed: {e}"),
            })
        })
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        if len == 0 {
            return Ok(Box::new(tokio::io::empty()));
        }
        let resp = self.send_content_request(offset, len).await?;
        let stream = resp.bytes_stream().map_err(|e| {
            error!(error = %e, "Dropbox stream error");
            std::io::Error::other(transport_error(DropboxError::Api {
                status: 0,
                detail: format!("stream read failed: {e}"),
            }))
        });
        Ok(Box::new(tokio_util::io::StreamReader::new(stream)))
    }
}

fn transport_error(e: DropboxError) -> ReadError {
    ReadError::Transport(Box::new(e))
}

fn classify_api_error(status: u16, body: &str, shared_url: &str) -> DropboxError {
    let tag = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|j| j["error"][".tag"].as_str().map(String::from));

    match status {
        401 => DropboxError::InvalidCredentials,
        409 => match tag.as_deref() {
            Some("shared_link_not_found") => DropboxError::FileNotFound {
                url: shared_url.to_string(),
            },
            Some("shared_link_access_denied") | Some("shared_link_is_directory") => {
                DropboxError::NotPublic {
                    url: shared_url.to_string(),
                }
            }
            _ => DropboxError::Api {
                status,
                detail: body.to_string(),
            },
        },
        429 => DropboxError::RateLimited {
            retry_after_secs: 60,
        },
        507 => DropboxError::QuotaExceeded,
        _ => DropboxError::Api {
            status,
            detail: body.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn size_returns_metadata_size() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip", "size": 1000, ".tag": "file"
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 1000);
    }

    #[tokio::test]
    async fn size_rejects_folder_tag() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "folder", "size": 0, ".tag": "folder"
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/folder".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::InvalidUrl { .. } => {}
                    _ => panic!("Expected InvalidUrl error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }

    #[tokio::test]
    async fn read_range_returns_bytes() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .and(header("Range", "bytes=0-9"))
            .respond_with(
                ResponseTemplate::new(206).set_body_raw(vec![42; 10], "application/octet-stream"),
            )
            .mount(&content)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let bytes = reader.read_range(0, 10).await.unwrap();
        assert_eq!(bytes.as_ref(), &[42u8; 10]);
    }

    #[tokio::test]
    async fn read_range_sends_basic_auth_and_api_arg_headers() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        let shared_url = "https://www.dropbox.com/s/abc/x.zip";
        let api_arg = serde_json::json!({ "url": shared_url }).to_string();

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .and(header("Authorization", "Basic a2V5OnNlY3JldA=="))
            .and(header("Dropbox-API-Arg", api_arg))
            .and(header("Range", "bytes=0-3"))
            .respond_with(ResponseTemplate::new(206).set_body_raw("test", "text/plain"))
            .mount(&content)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            shared_url.to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let bytes = reader.read_range(0, 4).await.unwrap();
        assert_eq!(bytes, "test");
    }

    #[tokio::test]
    async fn metadata_uses_basic_auth_header() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Basic a2V5OnNlY3JldA=="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip", "size": 42, ".tag": "file"
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 42);
    }

    #[tokio::test]
    async fn read_range_zero_len_returns_empty() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let bytes = reader.read_range(0, 0).await.unwrap();
        assert_eq!(bytes.len(), 0);
    }

    #[tokio::test]
    async fn read_stream_returns_bytes() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .respond_with(
                ResponseTemplate::new(206)
                    .set_body_raw("stream-content", "application/octet-stream"),
            )
            .mount(&content)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let mut stream = reader.read_stream(0, 14).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"stream-content");
    }

    #[tokio::test]
    async fn content_416_maps_to_out_of_bounds() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&content)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.read_range(1000, 10).await;
        match res {
            Err(ReadError::OutOfBounds { offset, len, .. }) => {
                assert_eq!(offset, 1000);
                assert_eq!(len, 10);
            }
            _ => panic!("Expected OutOfBounds error"),
        }
    }

    #[tokio::test]
    async fn metadata_401_tunnels_invalid_credentials() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error_summary": "invalid_access_token/",
                "error": { ".tag": "invalid_access_token" }
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::InvalidCredentials => {}
                    _ => panic!("Expected InvalidCredentials error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }

    #[tokio::test]
    async fn metadata_409_shared_link_not_found_tunnels_file_not_found() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error_summary": "shared_link_not_found/",
                "error": { ".tag": "shared_link_not_found" }
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::FileNotFound { .. } => {}
                    _ => panic!("Expected FileNotFound error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }

    #[tokio::test]
    async fn metadata_409_access_denied_tunnels_not_public() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error_summary": "shared_link_access_denied/",
                "error": { ".tag": "shared_link_access_denied" }
            })))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::NotPublic { .. } => {}
                    _ => panic!("Expected NotPublic error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }

    #[tokio::test]
    async fn metadata_429_tunnels_rate_limited() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::RateLimited { .. } => {}
                    _ => panic!("Expected RateLimited error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }

    #[tokio::test]
    async fn metadata_507_tunnels_quota_exceeded() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(507))
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            "key".to_string(),
            "secret".to_string(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.size().await;
        match res {
            Err(ReadError::Transport(e)) => {
                let de = e.downcast_ref::<DropboxError>().unwrap();
                match de {
                    DropboxError::QuotaExceeded => {}
                    _ => panic!("Expected QuotaExceeded error"),
                }
            }
            _ => panic!("Expected Transport error"),
        }
    }
}
