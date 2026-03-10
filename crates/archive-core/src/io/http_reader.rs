use bytes::Bytes;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_LENGTH, HeaderMap, HeaderValue, RANGE},
};

use super::ReadAt;
use crate::ArchiveError;

/// A [`ReadAt`] backed by HTTP range requests.
///
/// Each call to [`read_range`](ReadAt::read_range) sends a GET with a
/// `Range: bytes=start-end` header.  The server **must** support range
/// requests (respond with `206 Partial Content`).
///
/// An optional set of extra headers can be injected at construction time
/// (e.g. `Authorization: Bearer <token>` for cloud storage APIs).
pub struct HttpReadAt {
    url: String,
    client: Client,
    extra_headers: HeaderMap,
}

impl HttpReadAt {
    /// Create a new `HttpReadAt` targeting `url`.
    ///
    /// Uses the provided [`Client`] so the caller can configure timeouts,
    /// connection pooling, etc.
    pub fn new(url: impl Into<String>, client: Client) -> Self {
        Self {
            url: url.into(),
            client,
            extra_headers: HeaderMap::new(),
        }
    }

    /// Attach extra headers that will be sent with every request.
    ///
    /// Useful for injecting OAuth tokens:
    /// ```ignore
    /// reader.with_headers(headers);
    /// ```
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.extra_headers = headers;
        self
    }
}

impl ReadAt for HttpReadAt {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ArchiveError> {
        if len == 0 {
            return Ok(Bytes::new());
        }

        // Range header is inclusive on both ends: "bytes=0-9" returns 10 bytes.
        let range_end = offset + len - 1;
        let range_value = format!("bytes={offset}-{range_end}");

        let response = self
            .client
            .get(&self.url)
            .headers(self.extra_headers.clone())
            .header(
                RANGE,
                HeaderValue::from_str(&range_value)
                    .map_err(|e| ArchiveError::Other(format!("invalid range header: {e}")))?,
            )
            .send()
            .await
            .map_err(|e| ArchiveError::Other(format!("HTTP request failed: {e}")))?;

        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let body = response
                    .bytes()
                    .await
                    .map_err(|e| ArchiveError::Other(format!("failed to read body: {e}")))?;
                Ok(body)
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                // Server told us the range is invalid — map to OutOfBounds.
                Err(ArchiveError::OutOfBounds {
                    offset,
                    len,
                    size: 0, // We don't know the real size from a 416 alone.
                })
            }
            status => Err(ArchiveError::Other(format!(
                "unexpected HTTP status {status} for range request"
            ))),
        }
    }

    async fn size(&self) -> Result<u64, ArchiveError> {
        let response = self
            .client
            .head(&self.url)
            .headers(self.extra_headers.clone())
            .send()
            .await
            .map_err(|e| ArchiveError::Other(format!("HEAD request failed: {e}")))?;

        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .ok_or_else(|| {
                ArchiveError::Other("server did not return Content-Length header".into())
            })?
            .to_str()
            .map_err(|e| ArchiveError::Other(format!("invalid Content-Length: {e}")))?
            .parse::<u64>()
            .map_err(|e| ArchiveError::Other(format!("Content-Length not a number: {e}")))?;

        Ok(content_length)
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
    async fn range_read_returns_correct_bytes() {
        let server = MockServer::start().await;
        let full_data = b"hello archive sniper over http";

        // Mock: GET with Range header → 206 + slice of data
        Mock::given(method("GET"))
            .and(header("range", "bytes=6-12"))
            .respond_with(
                ResponseTemplate::new(206).set_body_bytes(&full_data[6..13]), // "archive"
            )
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let result = reader.read_range(6, 7).await.unwrap();

        assert_eq!(result.as_ref(), b"archive");
    }

    #[tokio::test]
    async fn size_from_head_request() {
        let server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-length", "42000"))
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        assert_eq!(reader.size().await.unwrap(), 42000);
    }

    #[tokio::test]
    async fn server_returns_416_maps_to_out_of_bounds() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let err = reader.read_range(9999, 100).await.unwrap_err();

        assert!(
            matches!(err, ArchiveError::OutOfBounds { .. }),
            "expected OutOfBounds, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn zero_length_read_returns_empty() {
        // Should not even hit the server.
        let reader = HttpReadAt::new("http://localhost:1", Client::new());
        let result = reader.read_range(0, 0).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn extra_headers_are_sent() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(header("authorization", "Bearer test-token-123"))
            .and(header("range", "bytes=0-3"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(b"test"))
            .mount(&server)
            .await;

        let mut auth_headers = HeaderMap::new();
        auth_headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer test-token-123"),
        );

        let reader = HttpReadAt::new(server.uri(), Client::new()).with_headers(auth_headers);

        let result = reader.read_range(0, 4).await.unwrap();
        assert_eq!(result.as_ref(), b"test");
    }
}
