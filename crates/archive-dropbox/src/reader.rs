use archive_core::io::{ReadAt, ReadError};
use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::Client;
use tokio::io::AsyncRead;
use tokio::sync::OnceCell;
use tracing::{debug, error, info, trace};

use crate::auth::DropboxAuth;
use crate::error::DropboxError;

pub struct DropboxReadAt {
    shared_url: String,
    auth: DropboxAuth,
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
    pub fn new(shared_url: String, auth: DropboxAuth) -> Self {
        Self::build(
            shared_url,
            auth,
            "https://api.dropboxapi.com",
            "https://content.dropboxapi.com",
        )
    }

    fn build(shared_url: String, auth: DropboxAuth, api_base: &str, content_base: &str) -> Self {
        let client = Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        Self {
            shared_url,
            auth,
            client,
            api_base_url: api_base.to_string(),
            content_base_url: content_base.to_string(),
            metadata: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_base_urls(
        shared_url: String,
        auth: DropboxAuth,
        api_base: &str,
        content_base: &str,
    ) -> Self {
        Self::build(shared_url, auth, api_base, content_base)
    }

    // ── Auth helpers ────────────────────────────────────────────────────────

    /// Apply the correct auth header to a request builder.
    /// `AppCredentials` → Basic; `OAuth` → Bearer.
    async fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            DropboxAuth::AppCredentials {
                app_key,
                app_secret,
            } => builder.basic_auth(app_key, Some(app_secret)),
            DropboxAuth::OAuth { token, .. } => {
                let access = token.read().await.access_token.clone();
                builder.bearer_auth(access)
            }
        }
    }

    /// Refresh the OAuth access token on a 401, avoiding thundering herd.
    ///
    /// Checks under the write lock whether another task already refreshed.
    /// If so, returns the new token without issuing another refresh request.
    async fn refresh_oauth(&self, seen_access: &str) -> Result<String, ReadError> {
        let DropboxAuth::OAuth {
            token,
            store,
            client_id,
            client_secret,
            config,
        } = &self.auth
        else {
            // Should never be called on AppCredentials path.
            return Err(transport_error(DropboxError::TokenRefreshFailed {
                reason: "refresh called on non-OAuth auth".into(),
            }));
        };

        let mut w = token.write().await;

        // Another task beat us to the refresh.
        if w.access_token != seen_access {
            return Ok(w.access_token.clone());
        }

        let refresh_tok = w.refresh_token.clone().ok_or_else(|| {
            transport_error(DropboxError::TokenRefreshFailed {
                reason: "no refresh token available".into(),
            })
        })?;

        let new = archive_auth::refresh(config, client_id, client_secret.as_deref(), &refresh_tok)
            .await
            .map_err(|e| {
                transport_error(DropboxError::TokenRefreshFailed {
                    reason: e.to_string(),
                })
            })?;

        store.save("dropbox", new.clone()).map_err(|e| {
            transport_error(DropboxError::TokenRefreshFailed {
                reason: format!("failed to persist token: {e}"),
            })
        })?;

        *w = new;
        Ok(w.access_token.clone())
    }

    // ── Metadata ────────────────────────────────────────────────────────────

    async fn ensure_metadata(&self) -> Result<&DropboxMetadata, ReadError> {
        self.metadata
            .get_or_try_init(|| self.fetch_metadata())
            .await
    }

    async fn fetch_metadata(&self) -> Result<DropboxMetadata, ReadError> {
        debug!(url = %self.shared_url, "Fetching Dropbox shared link metadata");
        let url = format!("{}/2/sharing/get_shared_link_metadata", self.api_base_url);
        let body = serde_json::json!({ "url": self.shared_url });

        // Capture the current access token before the request so that if another
        // task refreshes it while we await, our `seen_access` still points to the
        // stale token and the thundering-herd guard in refresh_oauth can fire.
        let seen_access = if let DropboxAuth::OAuth { token, .. } = &self.auth {
            let t = token.read().await;
            Some(t.access_token.clone())
        } else {
            None
        };

        let req = self.apply_auth(self.client.post(&url)).await.json(&body);
        let resp = req.send().await.map_err(|e| {
            error!(error = %e, "Dropbox metadata request failed");
            transport_error(DropboxError::Api {
                status: 0,
                detail: format!("metadata request failed: {e}"),
            })
        })?;

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let (DropboxAuth::OAuth { .. }, Some(seen)) = (&self.auth, seen_access) {
                let new_access = self.refresh_oauth(&seen).await?;
                // Retry once with fresh token.
                let resp2 = self
                    .client
                    .post(&url)
                    .bearer_auth(&new_access)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| {
                        transport_error(DropboxError::Api {
                            status: 0,
                            detail: format!("metadata retry failed: {e}"),
                        })
                    })?;
                return self.parse_metadata_response(resp2).await;
            }
            // AppCredentials path: 401 means invalid credentials, no retry.
            let text = resp.text().await.unwrap_or_default();
            let err = classify_api_error(status.as_u16(), &text, &self.shared_url);
            error!(status = status.as_u16(), error = ?err, "Dropbox metadata API error");
            return Err(transport_error(err));
        }

        self.parse_metadata_response(resp).await
    }

    async fn parse_metadata_response(
        &self,
        resp: reqwest::Response,
    ) -> Result<DropboxMetadata, ReadError> {
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
    }

    // ── Content requests ────────────────────────────────────────────────────

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

        // Capture the current access token before the request so that if another
        // task refreshes it while we await, our `seen_access` still points to the
        // stale token and the thundering-herd guard in refresh_oauth can fire.
        let seen_access = if let DropboxAuth::OAuth { token, .. } = &self.auth {
            let t = token.read().await;
            Some(t.access_token.clone())
        } else {
            None
        };

        let req = self
            .apply_auth(self.client.post(&url))
            .await
            .header("Dropbox-API-Arg", &api_arg)
            .header("Range", format!("bytes={offset}-{range_end}"));

        let resp = req.send().await.map_err(|e| {
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

        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let (DropboxAuth::OAuth { .. }, Some(seen)) = (&self.auth, seen_access) {
                let new_access = self.refresh_oauth(&seen).await?;
                // Retry once with fresh token. Preserve all required headers.
                let resp2 = self
                    .client
                    .post(&url)
                    .bearer_auth(&new_access)
                    .header("Dropbox-API-Arg", &api_arg)
                    .header("Range", format!("bytes={offset}-{range_end}"))
                    .send()
                    .await
                    .map_err(|e| {
                        transport_error(DropboxError::Api {
                            status: 0,
                            detail: format!("content retry failed: {e}"),
                        })
                    })?;
                return self.check_content_response(resp2, offset, len).await;
            }
            // AppCredentials path: surface 401 as InvalidCredentials.
            let body = resp.text().await.unwrap_or_default();
            let err = classify_api_error(status.as_u16(), &body, &self.shared_url);
            error!(status = status.as_u16(), error = ?err, "Dropbox content API error");
            return Err(transport_error(err));
        }

        self.check_content_response(resp, offset, len).await
    }

    async fn check_content_response(
        &self,
        resp: reqwest::Response,
        offset: u64,
        len: u64,
    ) -> Result<reqwest::Response, ReadError> {
        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            return Ok(resp);
        }
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            error!(offset = offset, len = len, "Dropbox range not satisfiable");
            // Fetch real size from metadata so upper layers get an accurate OutOfBounds.
            let size = self.ensure_metadata().await.map(|m| m.size).unwrap_or(0);
            return Err(ReadError::OutOfBounds { offset, len, size });
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
    use std::sync::Arc;

    use archive_auth::{StoredToken, TokenStore};
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::DropboxAuth;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn app_creds() -> DropboxAuth {
        DropboxAuth::app_credentials("key".into(), "secret".into())
    }

    fn temp_store() -> (TempDir, Arc<TokenStore>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(TokenStore::load_from(dir.path().join("tokens.json")).unwrap());
        (dir, store)
    }

    fn make_oauth_auth(
        access_token: &str,
        refresh_token: Option<&str>,
        store: Arc<TokenStore>,
    ) -> DropboxAuth {
        make_oauth_auth_with_config(
            access_token,
            refresh_token,
            store,
            &archive_auth::DROPBOX_CONFIG,
        )
    }

    fn make_oauth_auth_with_config(
        access_token: &str,
        refresh_token: Option<&str>,
        store: Arc<TokenStore>,
        config: &'static archive_auth::ProviderOAuthConfig,
    ) -> DropboxAuth {
        DropboxAuth::oauth_with_config(
            StoredToken {
                access_token: access_token.to_string(),
                refresh_token: refresh_token.map(str::to_string),
                expires_at: None,
                scopes: vec![],
            },
            store,
            "client_id".into(),
            Some("client_secret".into()),
            config,
        )
    }

    /// Leak a mock token-endpoint URL into a static ProviderOAuthConfig.
    ///
    /// `Box::leak` is acceptable in tests — each test runs once and the process
    /// exits; the tiny allocation is never a concern.
    fn mock_config(
        token_ep: &MockServer,
        scopes: &'static [&'static str],
    ) -> &'static archive_auth::ProviderOAuthConfig {
        let token_url: &'static str =
            Box::leak(format!("{}/token", token_ep.uri()).into_boxed_str());
        Box::leak(Box::new(archive_auth::ProviderOAuthConfig {
            name: "dropbox",
            auth_url: "https://www.dropbox.com/oauth2/authorize",
            token_url,
            scopes,
            extra_auth_params: &[],
        }))
    }

    // ── AppCredentials path (existing tests updated) ──────────────────────

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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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

        // Metadata mock so ensure_metadata succeeds and returns a real size.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip", "size": 5000, ".tag": "file"
            })))
            .mount(&api)
            .await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&content)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            app_creds(),
            &api.uri(),
            &content.uri(),
        );
        let res = reader.read_range(1000, 10).await;
        match res {
            Err(ReadError::OutOfBounds { offset, len, size }) => {
                assert_eq!(offset, 1000);
                assert_eq!(len, 10);
                assert_eq!(size, 5000);
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
            .expect(1) // AppCredentials must NOT retry on 401.
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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
            app_creds(),
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

    // ── OAuth path (new tests) ────────────────────────────────────────────

    #[tokio::test]
    async fn oauth_bearer_header_sent() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "test.zip", "size": 500, ".tag": "file"
            })))
            .mount(&api)
            .await;

        let (_dir, store) = temp_store();
        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth("initial_token", Some("refresh_tok"), store),
            &api.uri(),
            &content.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 500);
    }

    #[tokio::test]
    async fn oauth_refresh_on_401() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // First size() call: initial_token → 401.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&api)
            .await;

        // Retry after refresh: new_access_token → 200 with metadata.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer new_access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "test.zip", "size": 999, ".tag": "file"
            })))
            .expect(1)
            .mount(&api)
            .await;

        // Token endpoint returns new access token — called exactly once.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new_access_token",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&token_ep)
            .await;

        let (_dir, store) = temp_store();
        let config = mock_config(&token_ep, &[]);

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth_with_config(
                "initial_token",
                Some("refresh_tok"),
                store.clone(),
                config,
            ),
            &api.uri(),
            &content.uri(),
        );

        let size = reader.size().await.unwrap();
        assert_eq!(size, 999);

        // Store must have been updated with the new access token.
        assert_eq!(
            store.get("dropbox").unwrap().access_token,
            "new_access_token"
        );
        // wiremock verifies expect(1) on each mock at drop.
    }

    #[tokio::test]
    async fn oauth_refresh_thundering_herd_avoided() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // Both initial read_range calls hit metadata fetch + 401 on content.
        // Metadata mock — returns 200 immediately.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip", "size": 10, ".tag": "file"
            })))
            .mount(&api)
            .await;

        // Both initial content requests hit 401.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .and(header("Authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&content)
            .await;

        // After refresh, content server serves byte ranges.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .and(header("Authorization", "Bearer refreshed_token"))
            .and(header("Range", "bytes=0-4"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"hello"))
            .mount(&content)
            .await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_file"))
            .and(header("Authorization", "Bearer refreshed_token"))
            .and(header("Range", "bytes=5-9"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"world"))
            .mount(&content)
            .await;

        // Token endpoint — must be called exactly once despite two concurrent 401s.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "refreshed_token",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&token_ep)
            .await;

        let (_dir, store) = temp_store();
        let config = mock_config(&token_ep, &[]);

        // Both tasks share the same reader (and therefore the same RwLock<StoredToken>).
        let reader = std::sync::Arc::new(DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth_with_config("initial_token", Some("refresh_tok"), store, config),
            &api.uri(),
            &content.uri(),
        ));

        let r1 = reader.clone();
        let r2 = reader.clone();

        // Fire two concurrent reads — exactly one token refresh must occur.
        let (res1, res2) = tokio::join!(async move { r1.read_range(0, 5).await }, async move {
            r2.read_range(5, 5).await
        },);

        assert_eq!(res1.unwrap().as_ref(), b"hello");
        assert_eq!(res2.unwrap().as_ref(), b"world");
        // wiremock verifies expect(1) on token_ep at drop.
    }

    #[tokio::test]
    async fn oauth_refresh_failure_returns_transport_error() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // API returns 401, triggering a refresh attempt.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&api)
            .await;

        // Token endpoint returns 400 — refresh fails.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked."
            })))
            .mount(&token_ep)
            .await;

        let (_dir, store) = temp_store();
        let config = mock_config(&token_ep, &[]);

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth_with_config("initial_token", Some("bad_refresh"), store, config),
            &api.uri(),
            &content.uri(),
        );

        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport error, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<DropboxError>(),
                Some(DropboxError::TokenRefreshFailed { .. })
            ),
            "expected TokenRefreshFailed, got: {:?}",
            e.downcast_ref::<DropboxError>()
        );
    }

    #[tokio::test]
    async fn oauth_no_refresh_token_returns_transport_error() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        // API always returns 401.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&api)
            .await;

        let (_dir, store) = temp_store();
        // StoredToken has no refresh_token.
        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth("initial_token", None, store),
            &api.uri(),
            &content.uri(),
        );

        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport error, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<DropboxError>(),
                Some(DropboxError::TokenRefreshFailed { .. })
            ),
            "expected TokenRefreshFailed, got: {:?}",
            e.downcast_ref::<DropboxError>()
        );
    }

    #[tokio::test]
    async fn app_credentials_path_does_not_retry_on_401() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;

        // The mock expects exactly 1 call — no retry.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error_summary": "invalid_access_token/",
                "error": { ".tag": "invalid_access_token" }
            })))
            .expect(1)
            .mount(&api)
            .await;

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            app_creds(),
            &api.uri(),
            &content.uri(),
        );
        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<DropboxError>(),
                Some(DropboxError::InvalidCredentials)
            ),
            "expected InvalidCredentials, got: {:?}",
            e.downcast_ref::<DropboxError>()
        );
        // wiremock will verify expect(1) on drop.
    }

    #[tokio::test]
    async fn refresh_persists_to_store() {
        let api = MockServer::start().await;
        let content = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // API: initial 401 with old token, then success with persisted token.
        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer old_token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&api)
            .await;

        Mock::given(method("POST"))
            .and(path("/2/sharing/get_shared_link_metadata"))
            .and(header("Authorization", "Bearer persisted_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip", "size": 42, ".tag": "file"
            })))
            .mount(&api)
            .await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "persisted_token",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&token_ep)
            .await;

        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("tokens.json");
        let store = Arc::new(TokenStore::load_from(store_path.clone()).unwrap());
        let config = mock_config(&token_ep, &[]);

        let reader = DropboxReadAt::new_with_base_urls(
            "https://www.dropbox.com/s/abc/x.zip".to_string(),
            make_oauth_auth_with_config("old_token", Some("old_refresh_tok"), store, config),
            &api.uri(),
            &content.uri(),
        );

        assert_eq!(reader.size().await.unwrap(), 42);

        // Drop reader, reload store from disk, verify token was persisted.
        drop(reader);
        let store2 = TokenStore::load_from(store_path).unwrap();
        assert_eq!(
            store2.get("dropbox").unwrap().access_token,
            "persisted_token"
        );
    }
}
