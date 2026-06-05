//! Shared browser-picker plumbing.
//!
//! Provider crates build provider-specific HTML (Google Picker / Dropbox Chooser),
//! hand it to [`serve_picker_page`], and parse the returned POST body into
//! [`PickedFile`]s. This module owns only the transport: bind a loopback port,
//! open the browser, serve the page, and capture the first `POST /picked`.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

use crate::error::AuthError;

/// A file the user picked in the browser, mapped to a source string `archive-source::resolve()`
/// already accepts (a provider URL). Provider-neutral — the TUI builds a bookmark from it without
/// knowing which provider produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedFile {
    /// Resolvable source string (e.g. `https://drive.google.com/open?id=…` or a Dropbox share link).
    pub source: String,
    /// Display name (file name as shown by the provider).
    pub name: String,
    /// Size in bytes (0 if the provider did not report one).
    pub size: u64,
}

/// Resolve the loopback port — same convention as the OAuth flow (`flow.rs`).
/// Default 53682; overridable via `PLUQ_OAUTH_PORT`.
fn loopback_port() -> u16 {
    const DEFAULT_LOOPBACK_PORT: u16 = 53682;
    std::env::var("PLUQ_OAUTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_LOOPBACK_PORT)
}

/// Serve `html` on a loopback port, open the browser to it, and return the body of the first
/// `POST /picked` the page sends back. Times out after `timeout_secs`; Ctrl-C cancels.
///
/// The returned `String` is the raw POST body (provider crates parse it as JSON).
pub async fn serve_picker_page(html: String, timeout_secs: u64) -> Result<String, AuthError> {
    let port = loopback_port();
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        AuthError::Callback(format!(
            "could not bind 127.0.0.1:{port} for the file picker — another auth/pick flow may be \
             running, or the port is in use. Close it and retry, or set PLUQ_OAUTH_PORT. ({e})"
        ))
    })?;

    // Use `localhost` (resolves to 127.0.0.1, which we own) so the served page and its POST-back
    // share one origin.
    let url = format!("http://localhost:{port}/");
    if let Err(e) = webbrowser::open(&url) {
        eprintln!("Failed to open browser: {e}. Please open this URL manually:");
        eprintln!("{url}");
    }

    serve_and_capture(listener, html, timeout_secs).await
}

/// Core loop, split out so tests can drive it with a pre-bound listener and no browser launch.
async fn serve_and_capture(
    listener: TcpListener,
    html: String,
    timeout_secs: u64,
) -> Result<String, AuthError> {
    let server_fut = timeout(Duration::from_secs(timeout_secs), async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|e| AuthError::Callback(e.to_string()))?;

            // CRITICAL: a per-connection error must NOT kill the server. Browsers open speculative /
            // parallel sockets (preconnect, favicon, prefetch) and may drop them without a full
            // request — that surfaces here as an `Err` (e.g. "connection closed before headers").
            // Log and move to the next connection; only the timeout / Ctrl-C ends the loop.
            let (method, path, body) = match read_http_request(&mut stream).await {
                Ok(req) => req,
                Err(e) => {
                    tracing::warn!("picker: ignoring bad connection: {e}");
                    continue;
                }
            };

            if method == "POST" && path == "/picked" {
                // Body is already captured; the ack write is best-effort (don't fail the pick if the
                // browser already closed the socket).
                let _ = write_response(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    "<html><body><h1>Done. You can close this tab.</h1></body></html>",
                )
                .await;
                return Ok::<String, AuthError>(body);
            } else if method == "GET" && path == "/" {
                let _ =
                    write_response(&mut stream, "200 OK", "text/html; charset=utf-8", &html).await;
                // keep looping — the POST-back arrives on a later connection
            } else {
                let _ = write_response(&mut stream, "404 Not Found", "text/plain", "").await;
            }
        }
    });

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => Err(AuthError::Cancelled),
        res = server_fut => res.map_err(|_| AuthError::Timeout { secs: timeout_secs })?,
    }
}

/// Read one HTTP request: returns (method, path, body). Reads headers fully, then reads exactly
/// `Content-Length` body bytes (handles bodies that span multiple reads).
async fn read_http_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<(String, String, String), AuthError> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];

    // Read until we have the full header block (terminated by CRLFCRLF).
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;
        if n == 0 {
            return Err(AuthError::Callback(
                "connection closed before headers".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| AuthError::Callback("empty request".into()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    // Body bytes already read past the header terminator.
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok((method, path, String::from_utf8_lossy(&body).to_string()))
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| AuthError::Callback(e.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|e| AuthError::Callback(e.to_string()))?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*; // brings AsyncReadExt/AsyncWriteExt (used below) — do NOT re-import them
    use tokio::net::TcpStream;

    /// Drive `serve_and_capture` directly (no browser): client does GET / (gets the page),
    /// then POST /picked with a JSON body, and we assert the captured body matches.
    #[tokio::test]
    async fn serves_page_then_captures_post_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            serve_and_capture(listener, "<html>PAGE</html>".to_string(), 10).await
        });

        // 1. GET / — expect the served HTML.
        let mut c1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c1.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        c1.read_to_string(&mut resp).await.unwrap();
        assert!(
            resp.contains("<html>PAGE</html>"),
            "GET should serve page: {resp}"
        );

        // 2. POST /picked — body should be returned by serve_and_capture.
        let payload = r#"[{"id":"abc","name":"x.zip","size":10}]"#;
        let req = format!(
            "POST /picked HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let mut c2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c2.write_all(req.as_bytes()).await.unwrap();
        let mut ack = String::new();
        c2.read_to_string(&mut ack).await.unwrap();
        assert!(ack.contains("200 OK"), "POST should be acked: {ack}");

        let captured = server.await.unwrap().unwrap();
        assert_eq!(captured, payload);
    }

    #[tokio::test]
    async fn unknown_path_404s_then_keeps_serving() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server =
            tokio::spawn(async move { serve_and_capture(listener, "PAGE".to_string(), 10).await });

        // Hit an unknown GET path — must 404, loop continues.
        let mut c1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c1.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        c1.read_to_string(&mut resp).await.unwrap();
        assert!(resp.contains("404"), "unknown path should 404: {resp}");

        // Then a valid POST still works.
        let payload = "[]";
        let req = format!(
            "POST /picked HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let mut c2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c2.write_all(req.as_bytes()).await.unwrap();
        let mut ack = String::new();
        c2.read_to_string(&mut ack).await.unwrap();

        let captured = server.await.unwrap().unwrap();
        assert_eq!(captured, "[]");
    }

    #[tokio::test]
    async fn dropped_preconnect_does_not_kill_server() {
        // Simulate a browser speculative socket: connect and immediately close, sending no request.
        // The server must survive (not return Err) and still capture a later POST.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server =
            tokio::spawn(async move { serve_and_capture(listener, "PAGE".to_string(), 10).await });

        // Preconnect-then-drop (no bytes sent).
        let c0 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        drop(c0);

        // A real POST afterward must still be captured.
        let payload = r#"[{"x":1}]"#;
        let req = format!(
            "POST /picked HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let mut c2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c2.write_all(req.as_bytes()).await.unwrap();
        let mut ack = String::new();
        c2.read_to_string(&mut ack).await.unwrap();

        let captured = server.await.unwrap().unwrap();
        assert_eq!(captured, payload);
    }
}
