use bytes::Bytes;
use futures_util::TryStreamExt;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_LENGTH, HeaderMap, HeaderValue, RANGE},
};
use tokio::io::AsyncRead;
use tokio_util::io::StreamReader;
use tracing::{debug, trace, warn};

use super::ReadAt;
use super::read_error::{NetworkPhase, ReadErrorKind, ReadFailure, ReadOperation, SourceInfo};
use crate::io::ReadError;

/// A [`ReadAt`] backed by HTTP range requests.
///
/// Each call to [`read_range`](ReadAt::read_range) sends a GET with a
/// `Range` header and collects the full response body into `Bytes`.
///
/// Each call to [`read_stream`](ReadAt::read_stream) sends the same GET
/// but returns the response body as a streaming [`AsyncRead`] — zero-copy
/// for large entry data extraction.
///
/// An optional set of extra headers can be injected at construction time
/// (e.g. `Authorization: Bearer <token>` for cloud storage APIs).
///
/// Stateless and dumb: no redirect resolution, no retries, no auth refresh.
/// Those are product-level concerns handled via [`ReadAt`] decorators.
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

/// Classify a `reqwest::Error` into a [`ReadError`].
fn classify_reqwest_error(e: reqwest::Error, url: &str, op: ReadOperation) -> ReadError {
    warn!(url = %url, operation = ?op, error = %e, "HTTP request failed");
    let kind = if e.is_timeout() {
        ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Transfer,
        }
    } else if e.is_connect() {
        ReadErrorKind::NetworkReset
    } else if let Some(status) = e.status() {
        ReadErrorKind::HttpStatus {
            code: status.as_u16(),
            headers: vec![],
            body: None,
        }
    } else {
        ReadErrorKind::NetworkReset
    };

    ReadError::Failed(Box::new(ReadFailure {
        source: SourceInfo::Http {
            url: url.to_string(),
        },
        operation: op,
        kind,
        diagnostic: e.to_string(),
    }))
}

/// Build and send a Range request, returning the in-flight response.
/// Used by both `read_range` (which collects) and `read_stream` (which streams).
async fn send_range_request(
    client: &Client,
    url: &str,
    extra_headers: &HeaderMap,
    offset: u64,
    len: u64,
) -> Result<reqwest::Response, ReadError> {
    let range_end = offset + len - 1;
    let range_value = format!("bytes={offset}-{range_end}");
    let op = ReadOperation::ReadRange { offset, len };

    trace!(url = %url, offset, len, range = %range_value, "sending range request");

    let response = client
        .get(url)
        .headers(extra_headers.clone())
        .header(
            RANGE,
            HeaderValue::from_str(&range_value).map_err(|e| {
                ReadError::Failed(Box::new(ReadFailure {
                    source: SourceInfo::Http {
                        url: url.to_string(),
                    },
                    operation: op.clone(),
                    kind: ReadErrorKind::NetworkReset,
                    diagnostic: format!("invalid range header: {e}"),
                }))
            })?,
        )
        .send()
        .await
        .map_err(|e| classify_reqwest_error(e, url, op.clone()))?;

    match response.status() {
        StatusCode::PARTIAL_CONTENT => Ok(response),
        StatusCode::RANGE_NOT_SATISFIABLE => Err(ReadError::OutOfBounds {
            offset,
            len,
            size: 0,
        }),
        status => {
            // Capture full server response for diagnostics — invaluable when
            // cloud providers return HTML error pages or JSON error bodies.
            let resp_headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string()))
                .collect();
            let resp_body = response.bytes().await.ok();

            warn!(
                url = %url,
                status = status.as_u16(),
                headers = ?resp_headers,
                body = resp_body.as_ref().map(|b| String::from_utf8_lossy(&b[..b.len().min(2048)])).as_deref().unwrap_or("<read failed>"),
                "unexpected HTTP status for range request"
            );

            Err(ReadError::Failed(Box::new(ReadFailure {
                source: SourceInfo::Http {
                    url: url.to_string(),
                },
                operation: op,
                kind: ReadErrorKind::HttpStatus {
                    code: status.as_u16(),
                    headers: resp_headers,
                    body: resp_body,
                },
                diagnostic: format!("unexpected HTTP status {status} for range request"),
            })))
        }
    }
}

impl ReadAt for HttpReadAt {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        if len == 0 {
            return Ok(Bytes::new());
        }

        debug!(offset, len, "http read_range start");
        let op = ReadOperation::ReadRange { offset, len };
        let response =
            send_range_request(&self.client, &self.url, &self.extra_headers, offset, len).await?;

        let body = response
            .bytes()
            .await
            .map_err(|e| classify_reqwest_error(e, &self.url, op))?;

        trace!(
            offset,
            len,
            received = body.len(),
            "http read_range complete"
        );
        Ok(body)
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        if len == 0 {
            return Ok(Box::new(tokio::io::empty()) as Box<dyn AsyncRead + Send + Unpin>);
        }

        debug!(offset, len, "http read_stream start");
        let response =
            send_range_request(&self.client, &self.url, &self.extra_headers, offset, len).await?;

        let url = self.url.clone();
        let stream = response.bytes_stream().map_err(move |e| {
            let read_error =
                classify_reqwest_error(e, &url, ReadOperation::ReadRange { offset, len });
            std::io::Error::other(read_error)
        });
        let async_read = StreamReader::new(stream);

        Ok(Box::new(async_read) as Box<dyn AsyncRead + Send + Unpin>)
    }

    async fn size(&self) -> Result<u64, ReadError> {
        debug!("http size check (HEAD request)");
        let op = ReadOperation::SizeCheck;

        let response = self
            .client
            .head(&self.url)
            .headers(self.extra_headers.clone())
            .send()
            .await
            .map_err(|e| classify_reqwest_error(e, &self.url, op.clone()))?;

        // Log all HEAD response headers at debug — essential for diagnosing
        // cloud provider quirks (missing Content-Length, wrong Accept-Ranges, etc.)
        if tracing::enabled!(tracing::Level::DEBUG) {
            let head_headers: Vec<(&str, &str)> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str(), v.to_str().unwrap_or("<binary>")))
                .collect();
            debug!(
                status = response.status().as_u16(),
                headers = ?head_headers,
                "HEAD response"
            );
        }

        // Fail early if the server explicitly declares no range support.
        if let Some(accept_ranges) = response.headers().get("accept-ranges")
            && accept_ranges.as_bytes() == b"none"
        {
            warn!(url = %self.url, "server returned Accept-Ranges: none");
            return Err(ReadError::Failed(Box::new(ReadFailure {
                source: SourceInfo::Http {
                    url: self.url.clone(),
                },
                operation: op.clone(),
                kind: ReadErrorKind::RangeNotSupported,
                diagnostic:
                    "server returned Accept-Ranges: none — surgical extraction is not possible"
                        .into(),
            })));
        }

        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .ok_or_else(|| {
                warn!(url = %self.url, "server did not return Content-Length header");
                ReadError::Failed(Box::new(ReadFailure {
                    source: SourceInfo::Http {
                        url: self.url.clone(),
                    },
                    operation: op.clone(),
                    kind: ReadErrorKind::RangeNotSupported,
                    diagnostic: "server did not return Content-Length header".into(),
                }))
            })?
            .to_str()
            .map_err(|e| {
                ReadError::Failed(Box::new(ReadFailure {
                    source: SourceInfo::Http {
                        url: self.url.clone(),
                    },
                    operation: op.clone(),
                    kind: ReadErrorKind::RangeNotSupported,
                    diagnostic: format!("invalid Content-Length: {e}"),
                }))
            })?
            .parse::<u64>()
            .map_err(|e| {
                ReadError::Failed(Box::new(ReadFailure {
                    source: SourceInfo::Http {
                        url: self.url.clone(),
                    },
                    operation: op,
                    kind: ReadErrorKind::RangeNotSupported,
                    diagnostic: format!("Content-Length not a number: {e}"),
                }))
            })?;

        debug!(size = content_length, "http size check complete");
        Ok(content_length)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    use super::*;

    #[tokio::test]
    async fn range_read_returns_correct_bytes() {
        let server = MockServer::start().await;
        let full_data = b"hello archive sniper over http";

        Mock::given(method("GET"))
            .and(header("range", "bytes=6-12"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(&full_data[6..13]))
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let result = reader.read_range(6, 7).await.unwrap();

        assert_eq!(result.as_ref(), b"archive");
    }

    #[tokio::test]
    async fn range_read_zero_length_returns_empty() {
        let reader = HttpReadAt::new("http://localhost:1", Client::new());
        let result = reader.read_range(0, 0).await.unwrap();
        assert!(result.is_empty());
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
            matches!(err, ReadError::OutOfBounds { .. }),
            "expected OutOfBounds, got: {err:?}"
        );
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

    // ── read_stream tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn stream_read_returns_correct_bytes() {
        let server = MockServer::start().await;
        let full_data = b"hello archive sniper over http";

        Mock::given(method("GET"))
            .and(header("range", "bytes=6-12"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(&full_data[6..13]))
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let mut stream = reader.read_stream(6, 7).await.unwrap();
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();

        assert_eq!(collected, b"archive");
    }

    #[tokio::test]
    async fn stream_read_zero_length_returns_empty() {
        let reader = HttpReadAt::new("http://localhost:1", Client::new());
        let mut stream = reader.read_stream(0, 0).await.unwrap();
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();
        assert!(collected.is_empty());
    }

    #[tokio::test]
    async fn stream_server_returns_416_maps_to_out_of_bounds() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(416))
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let err = reader.read_stream(9999, 100).await.map(|_| ()).unwrap_err();

        assert!(
            matches!(err, ReadError::OutOfBounds { .. }),
            "expected OutOfBounds, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn accept_ranges_none_returns_range_not_supported() {
        let server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "1000")
                    .insert_header("accept-ranges", "none"),
            )
            .mount(&server)
            .await;

        let reader = HttpReadAt::new(server.uri(), Client::new());
        let err = reader.size().await.unwrap_err();

        assert!(
            matches!(
                err,
                ReadError::Failed(ref f) if matches!(f.kind, ReadErrorKind::RangeNotSupported)
            ),
            "expected RangeNotSupported, got: {err:?}"
        );
    }
}
