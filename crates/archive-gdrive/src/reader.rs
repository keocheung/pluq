use archive_core::fingerprint::HttpResponseMetadata;
use archive_core::io::{ReadAt, ReadError};
use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::Client;
use tokio::io::AsyncRead;
use tokio::sync::OnceCell;
use tracing::{debug, info, trace};

use crate::auth::GDriveAuth;
use crate::error::GDriveError;

pub struct GoogleDriveReadAt {
    file_id: String,
    auth: GDriveAuth,
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
    modified_time: Option<String>,
    head_revision_id: Option<String>,
}

impl GoogleDriveReadAt {
    pub fn new(file_id: String, auth: GDriveAuth) -> Self {
        Self::build(file_id, auth, "https://www.googleapis.com")
    }

    fn build(file_id: String, auth: GDriveAuth, base_url: &str) -> Self {
        let client = Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        Self {
            file_id,
            auth,
            client,
            api_base_url: base_url.to_string(),
            metadata: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_base_url(file_id: String, auth: GDriveAuth, base_url: &str) -> Self {
        Self::build(file_id, auth, base_url)
    }

    /// Build a URL with auth credentials applied.
    ///
    /// For ApiKey: appends `?key=K` (or `&key=K`) to the path_and_query.
    /// For OAuth: returns the plain URL (auth header is applied separately).
    fn authed_url(&self, path_and_query: &str) -> String {
        match &self.auth {
            GDriveAuth::ApiKey(k) => {
                let sep = if path_and_query.contains('?') {
                    '&'
                } else {
                    '?'
                };
                format!("{}{}{sep}key={}", self.api_base_url, path_and_query, k)
            }
            GDriveAuth::OAuth { .. } => {
                format!("{}{}", self.api_base_url, path_and_query)
            }
        }
    }

    pub fn captured_metadata(&self) -> Option<HttpResponseMetadata> {
        self.metadata.get().map(|m| HttpResponseMetadata {
            size: Some(m.size),
            etag: m.head_revision_id.clone(),
            last_modified: m.modified_time.clone(),
        })
    }

    /// Apply the Authorization header for OAuth, no-op for ApiKey.
    async fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            GDriveAuth::ApiKey(_) => builder,
            GDriveAuth::OAuth { token, .. } => {
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
        let GDriveAuth::OAuth {
            token,
            store,
            client_id,
            client_secret,
            config,
        } = &self.auth
        else {
            // Should never be called on ApiKey path.
            return Err(transport_error(GDriveError::InvalidApiKey));
        };

        let mut w = token.write().await;

        // Another task beat us to the refresh.
        if w.access_token != seen_access {
            return Ok(w.access_token.clone());
        }

        let refresh_tok = w.refresh_token.clone().ok_or_else(|| {
            transport_error(GDriveError::TokenRefreshFailed {
                reason: "no refresh token available".into(),
            })
        })?;

        let new = archive_auth::refresh(config, client_id, client_secret.as_deref(), &refresh_tok)
            .await
            .map_err(|e| {
                transport_error(GDriveError::TokenRefreshFailed {
                    reason: e.to_string(),
                })
            })?;

        store.save("gdrive", new.clone()).map_err(|e| {
            transport_error(GDriveError::TokenRefreshFailed {
                reason: format!("failed to persist token: {e}"),
            })
        })?;

        *w = new;
        Ok(w.access_token.clone())
    }

    // ── Metadata ────────────────────────────────────────────────────────────

    async fn ensure_metadata(&self) -> Result<&GDriveMetadata, ReadError> {
        self.metadata
            .get_or_try_init(|| self.fetch_metadata())
            .await
    }

    async fn fetch_metadata(&self) -> Result<GDriveMetadata, ReadError> {
        debug!(file_id = %self.file_id, "fetching Google Drive metadata");
        let path = format!(
            "/drive/v3/files/{}?fields=name,size,mimeType,modifiedTime,headRevisionId",
            self.file_id
        );
        let url = self.authed_url(&path);

        // Capture the current access token before the request so that if another
        // task refreshes it while we await, our `seen_access` still points to the
        // stale token and the thundering-herd guard in refresh_oauth can fire.
        let seen_access = if let GDriveAuth::OAuth { token, .. } = &self.auth {
            let t = token.read().await;
            Some(t.access_token.clone())
        } else {
            None
        };

        let req = self.apply_auth(self.client.get(&url)).await;
        let resp = req.send().await.map_err(|e| {
            transport_error(GDriveError::Api {
                status: 0,
                detail: format!("metadata request failed: {e}"),
            })
        })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let (GDriveAuth::OAuth { .. }, Some(seen)) = (&self.auth, seen_access) {
                let new_access = self.refresh_oauth(&seen).await?;
                // Retry once with fresh token.
                let url2 = self.authed_url(&path);
                let resp2 = self
                    .client
                    .get(&url2)
                    .bearer_auth(&new_access)
                    .send()
                    .await
                    .map_err(|e| {
                        transport_error(GDriveError::Api {
                            status: 0,
                            detail: format!("metadata retry failed: {e}"),
                        })
                    })?;
                return self.parse_metadata_response(resp2).await;
            }
            // ApiKey path: 401 means invalid key, no retry.
            let body = resp.text().await.unwrap_or_default();
            return Err(transport_error(classify_api_error(
                status.as_u16(),
                &body,
                &self.file_id,
            )));
        }

        self.parse_metadata_response(resp).await
    }

    async fn parse_metadata_response(
        &self,
        resp: reqwest::Response,
    ) -> Result<GDriveMetadata, ReadError> {
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
        let modified_time = json["modifiedTime"].as_str().map(|s| s.to_string());
        let head_revision_id = json["headRevisionId"].as_str().map(|s| s.to_string());

        info!(file_id = %self.file_id, name = %name, size, "Google Drive metadata OK");
        Ok(GDriveMetadata {
            name,
            size,
            modified_time,
            head_revision_id,
        })
    }

    // ── Content requests ────────────────────────────────────────────────────

    async fn send_content_request(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<reqwest::Response, ReadError> {
        trace!(offset, len, "requesting Google Drive file content range");
        let range_end = offset + len - 1;
        let path = format!("/drive/v3/files/{}?alt=media", self.file_id);
        let url = self.authed_url(&path);

        // Capture the current access token before the request so that if another
        // task refreshes it while we await, our `seen_access` still points to the
        // stale token and the thundering-herd guard in refresh_oauth can fire.
        let seen_access = if let GDriveAuth::OAuth { token, .. } = &self.auth {
            let t = token.read().await;
            Some(t.access_token.clone())
        } else {
            None
        };

        let req = self
            .apply_auth(self.client.get(&url))
            .await
            .header("Range", format!("bytes={offset}-{range_end}"));

        let resp = req.send().await.map_err(|e| {
            transport_error(GDriveError::Api {
                status: 0,
                detail: format!("content request failed: {e}"),
            })
        })?;

        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            return Ok(resp);
        }

        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let (GDriveAuth::OAuth { .. }, Some(seen)) = (&self.auth, seen_access) {
                let new_access = self.refresh_oauth(&seen).await?;
                // Retry once with fresh token.
                let url2 = self.authed_url(&path);
                let resp2 = self
                    .client
                    .get(&url2)
                    .bearer_auth(&new_access)
                    .header("Range", format!("bytes={offset}-{range_end}"))
                    .send()
                    .await
                    .map_err(|e| {
                        transport_error(GDriveError::Api {
                            status: 0,
                            detail: format!("content retry failed: {e}"),
                        })
                    })?;
                return self.check_content_response(resp2, offset, len).await;
            }
            // ApiKey path: surface 401 error directly.
            let body = resp.text().await.unwrap_or_default();
            return Err(transport_error(classify_api_error(
                status.as_u16(),
                &body,
                &self.file_id,
            )));
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
            // Fetch real size from metadata so upper layers get an accurate OutOfBounds.
            // This is an error path, so the extra request is acceptable.
            let size = self.ensure_metadata().await.map(|m| m.size).unwrap_or(0);
            return Err(ReadError::OutOfBounds { offset, len, size });
        }
        Err(transport_error(GDriveError::Api {
            status: status.as_u16(),
            detail: format!("unexpected status {status} for content request"),
        }))
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
        if len == 0 {
            return Ok(Bytes::new());
        }
        let resp = self.send_content_request(offset, len).await?;
        resp.bytes().await.map_err(|e| {
            transport_error(GDriveError::Api {
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
            std::io::Error::other(transport_error(GDriveError::Api {
                status: 0,
                detail: format!("stream read failed: {e}"),
            }))
        });
        Ok(Box::new(tokio_util::io::StreamReader::new(stream)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use archive_auth::{StoredToken, TokenStore};
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::*;
    use crate::auth::GDriveAuth;

    fn api_key_auth() -> GDriveAuth {
        GDriveAuth::ApiKey("test_key".into())
    }

    fn make_oauth_auth(
        access_token: &str,
        refresh_token: Option<&str>,
        store: Arc<TokenStore>,
    ) -> GDriveAuth {
        make_oauth_auth_with_config(
            access_token,
            refresh_token,
            store,
            &archive_auth::GDRIVE_CONFIG,
        )
    }

    /// Build an OAuth auth with a custom config — used to inject a mock token endpoint.
    fn make_oauth_auth_with_config(
        access_token: &str,
        refresh_token: Option<&str>,
        store: Arc<TokenStore>,
        config: &'static archive_auth::ProviderOAuthConfig,
    ) -> GDriveAuth {
        GDriveAuth::oauth_with_config(
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
            name: "gdrive",
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url,
            scopes,
            extra_auth_params: &[],
        }))
    }

    // ── ApiKey path (existing tests updated) ─────────────────────────────

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
            GDriveAuth::ApiKey("test_key".to_string()),
            &server.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 1000);
    }

    #[tokio::test]
    async fn read_range_delegates_to_inner() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("range", "bytes=0-9"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"0123456789"))
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "test_file_id".to_string(),
            GDriveAuth::ApiKey("test_key".to_string()),
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
            GDriveAuth::ApiKey("test_key".to_string()),
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
            GDriveAuth::ApiKey("test_key".to_string()),
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
            GDriveAuth::ApiKey("bad_key".to_string()),
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

    // ── OAuth path (new tests) ────────────────────────────────────────────

    fn temp_store() -> (TempDir, Arc<TokenStore>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(TokenStore::load_from(dir.path().join("tokens.json")).unwrap());
        (dir, store)
    }

    #[tokio::test]
    async fn oauth_bearer_header_sent() {
        let server = MockServer::start().await;
        // Drive API requires Bearer initial_token.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "test.zip",
                "size": "500",
                "mimeType": "application/zip"
            })))
            .mount(&server)
            .await;

        let (_dir, store) = temp_store();
        let reader = GoogleDriveReadAt::new_with_base_url(
            "file123".to_string(),
            make_oauth_auth("initial_token", Some("refresh_tok"), store),
            &server.uri(),
        );
        assert_eq!(reader.size().await.unwrap(), 500);
    }

    #[tokio::test]
    async fn oauth_refresh_on_401() {
        let drive = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // First size() call: initial_token → 401.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&drive)
            .await;

        // Retry after refresh: new_access_token → 200 with metadata.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer new_access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "test.zip",
                "size": "999",
                "mimeType": "application/zip"
            })))
            .expect(1)
            .mount(&drive)
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
        let config = mock_config(
            &token_ep,
            &["https://www.googleapis.com/auth/drive.readonly"],
        );

        let reader = GoogleDriveReadAt::new_with_base_url(
            "file123".to_string(),
            make_oauth_auth_with_config(
                "initial_token",
                Some("refresh_tok"),
                store.clone(),
                config,
            ),
            &drive.uri(),
        );

        // Reader drives the full 401 → refresh → retry path.
        let size = reader.size().await.unwrap();
        assert_eq!(size, 999);

        // Store must have been updated with the new access token.
        assert_eq!(
            store.get("gdrive").unwrap().access_token,
            "new_access_token"
        );
        // wiremock verifies expect(1) on each mock at drop.
    }

    #[tokio::test]
    async fn oauth_refresh_thundering_herd_avoided() {
        let drive = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // Both initial read_range calls hit 401 (same header).
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer initial_token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&drive)
            .await;

        // After refresh, Drive serves byte ranges.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer refreshed_token"))
            .and(header("range", "bytes=0-4"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"hello"))
            .mount(&drive)
            .await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer refreshed_token"))
            .and(header("range", "bytes=5-9"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"world"))
            .mount(&drive)
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
        let reader = std::sync::Arc::new(GoogleDriveReadAt::new_with_base_url(
            "file_id".to_string(),
            make_oauth_auth_with_config("initial_token", Some("refresh_tok"), store, config),
            &drive.uri(),
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
        let drive = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // Drive returns 401, triggering a refresh attempt.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&drive)
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

        let reader = GoogleDriveReadAt::new_with_base_url(
            "file_id".to_string(),
            make_oauth_auth_with_config("initial_token", Some("bad_refresh"), store, config),
            &drive.uri(),
        );

        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport error, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<GDriveError>(),
                Some(GDriveError::TokenRefreshFailed { .. })
            ),
            "expected TokenRefreshFailed, got: {:?}",
            e.downcast_ref::<GDriveError>()
        );
    }

    #[tokio::test]
    async fn oauth_no_refresh_token_returns_transport_error() {
        let drive = MockServer::start().await;

        // Drive always returns 401.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&drive)
            .await;

        let (_dir, store) = temp_store();
        // StoredToken has no refresh_token.
        let reader = GoogleDriveReadAt::new_with_base_url(
            "file123".to_string(),
            make_oauth_auth("initial_token", None, store),
            &drive.uri(),
        );

        let err = reader.size().await.unwrap_err();
        let ReadError::Transport(e) = err else {
            panic!("expected Transport error, got: {err:?}");
        };
        assert!(
            matches!(
                e.downcast_ref::<GDriveError>(),
                Some(GDriveError::TokenRefreshFailed { .. })
            ),
            "expected TokenRefreshFailed, got: {:?}",
            e.downcast_ref::<GDriveError>()
        );
    }

    #[tokio::test]
    async fn apikey_path_does_not_retry_on_401() {
        let server = MockServer::start().await;

        // The mock expects exactly 1 call — no retry.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {
                    "code": 401,
                    "message": "API key invalid.",
                    "errors": [{"reason": "authError"}]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "test_id".to_string(),
            api_key_auth(),
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
        // wiremock will verify expect(1) on drop.
    }

    #[tokio::test]
    async fn refresh_persists_to_store() {
        let drive = MockServer::start().await;
        let token_ep = MockServer::start().await;

        // Drive: initial 401, then success.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer old_token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&drive)
            .await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer persisted_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "x.zip",
                "size": "42",
                "mimeType": "application/zip"
            })))
            .mount(&drive)
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

        let reader = GoogleDriveReadAt::new_with_base_url(
            "file_id".to_string(),
            make_oauth_auth_with_config("old_token", Some("old_refresh_tok"), store, config),
            &drive.uri(),
        );

        // Drive size() through refresh pipeline.
        assert_eq!(reader.size().await.unwrap(), 42);

        // Drop reader, reload store from disk, verify token was persisted.
        drop(reader);
        let store2 = TokenStore::load_from(store_path).unwrap();
        assert_eq!(
            store2.get("gdrive").unwrap().access_token,
            "persisted_token"
        );
    }

    #[tokio::test]
    async fn read_stream_returns_bytes() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(header("range", "bytes=0-13"))
            .respond_with(
                ResponseTemplate::new(206)
                    .set_body_raw("stream-content", "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let reader = GoogleDriveReadAt::new_with_base_url(
            "file_id".to_string(),
            GDriveAuth::ApiKey("key".into()),
            &server.uri(),
        );
        let mut stream = reader.read_stream(0, 14).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"stream-content");
    }

    #[tokio::test]
    async fn read_range_zero_len_returns_empty() {
        let server = MockServer::start().await;
        let reader = GoogleDriveReadAt::new_with_base_url(
            "file_id".to_string(),
            GDriveAuth::ApiKey("key".into()),
            &server.uri(),
        );
        let bytes = reader.read_range(0, 0).await.unwrap();
        assert_eq!(bytes.len(), 0);
    }
}
