use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use url::Url;

use crate::error::AuthError;
use crate::providers::ProviderOAuthConfig;
use crate::store::StoredToken;

/// Run PKCE loopback OAuth flow against `config`.
///
/// TODO: flow integration test (requires real browser + provider)
pub async fn authenticate(
    config: &ProviderOAuthConfig,
    client_id: &str,
    client_secret: Option<&str>,
    timeout_secs: u64,
) -> Result<StoredToken, AuthError> {
    // 1. Bind ephemeral 127.0.0.1:0 HTTP listener.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_url = format!("http://127.0.0.1:{}/callback", port);

    // 2. Build oauth2::BasicClient
    let mut client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_auth_uri(
            AuthUrl::new(config.auth_url.to_string())
                .map_err(|e| AuthError::OAuth(e.to_string()))?,
        )
        .set_token_uri(
            TokenUrl::new(config.token_url.to_string())
                .map_err(|e| AuthError::OAuth(e.to_string()))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_url).map_err(|e| AuthError::OAuth(e.to_string()))?,
        );

    if let Some(secret) = client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.to_string()));
    }

    // PKCE
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // 3. Build authorization URL
    let mut auth_request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge);

    for scope in config.scopes {
        auth_request = auth_request.add_scope(Scope::new(scope.to_string()));
    }

    for (k, v) in config.extra_auth_params {
        auth_request = auth_request.add_extra_param(*k, *v);
    }

    let (auth_url, csrf_token) = auth_request.url();

    // 4. Open browser
    if let Err(e) = webbrowser::open(auth_url.as_str()) {
        eprintln!(
            "Failed to open browser: {}. Please open this URL manually:",
            e
        );
        eprintln!("{}", auth_url);
    }

    // 5. Receive callback — race the listener+timeout against Ctrl-C so an
    //    aborted flow exits immediately instead of hanging until the timeout.
    let server_fut = timeout(Duration::from_secs(timeout_secs), async {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;

        let mut buffer = [0; 4096];
        let n = stream
            .read(&mut buffer)
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;
        let request = String::from_utf8_lossy(&buffer[..n]);

        // Very basic HTTP parsing
        let first_line = request
            .lines()
            .next()
            .ok_or_else(|| AuthError::Callback("Empty request".to_string()))?;
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(AuthError::Callback("Invalid request line".to_string()));
        }

        let path = parts[1];
        let (code, state) = parse_callback_query(path)?;

        if state != *csrf_token.secret() {
            return Err(AuthError::OAuth("CSRF token mismatch".to_string()));
        }

        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
            <html><body><h1>Authenticated</h1><p>You can close this tab now.</p></body></html>";
        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|e| AuthError::Callback(e.to_string()))?;

        Ok::<String, AuthError>(code)
    });

    let callback_result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => return Err(AuthError::Cancelled),
        res = server_fut => res.map_err(|_| AuthError::Timeout { secs: timeout_secs })??,
    };

    // 6. Exchange code for tokens
    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AuthError::OAuth(e.to_string()))?;

    let token_result = client
        .exchange_code(AuthorizationCode::new(callback_result))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| AuthError::OAuth(e.to_string()))?;

    let expires_at = token_result.expires_in().map(|d| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + d.as_secs()
    });

    Ok(StoredToken {
        access_token: token_result.access_token().secret().to_string(),
        refresh_token: token_result.refresh_token().map(|r| r.secret().to_string()),
        expires_at,
        scopes: token_result
            .scopes()
            .map(|s| s.iter().map(|scope| scope.to_string()).collect())
            .unwrap_or_default(),
    })
}

/// Exchange a refresh token for a fresh access token.
///
/// Provider crates call this on 401. Returns a new `StoredToken` with
/// updated `access_token` + `expires_at`. `refresh_token` is preserved
/// (Google may omit it from refresh responses — fall back to the old one).
pub async fn refresh(
    config: &ProviderOAuthConfig,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<StoredToken, AuthError> {
    let mut client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_auth_uri(
            AuthUrl::new(config.auth_url.to_string())
                .map_err(|e| AuthError::OAuth(e.to_string()))?,
        )
        .set_token_uri(
            TokenUrl::new(config.token_url.to_string())
                .map_err(|e| AuthError::OAuth(e.to_string()))?,
        );

    if let Some(secret) = client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.to_string()));
    }

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AuthError::OAuth(e.to_string()))?;

    // Note: `extra_auth_params` (e.g. `access_type=offline`, `prompt=consent`) are
    // authorization-URL parameters for the browser redirect flow and have no meaning
    // in a token-endpoint refresh request — intentionally not forwarded here.
    let token_result = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http_client)
        .await
        .map_err(|e| AuthError::OAuth(e.to_string()))?;

    let expires_at = token_result.expires_in().map(|d| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + d.as_secs()
    });

    // Google may omit the refresh token on refresh responses — preserve the old one.
    let new_refresh_token = token_result
        .refresh_token()
        .map(|r| r.secret().to_string())
        .or_else(|| Some(refresh_token.to_string()));

    Ok(StoredToken {
        access_token: token_result.access_token().secret().to_string(),
        refresh_token: new_refresh_token,
        expires_at,
        scopes: token_result
            .scopes()
            .map(|s| s.iter().map(|scope| scope.to_string()).collect())
            .unwrap_or_default(),
    })
}

fn parse_callback_query(path: &str) -> Result<(String, String), AuthError> {
    let url = Url::parse(&format!("http://localhost{}", path))
        .map_err(|_| AuthError::Callback("Invalid path".to_string()))?;
    let pairs = url.query_pairs();

    let mut code = None;
    let mut state = None;

    for (k, v) in pairs {
        if k == "code" {
            code = Some(v.into_owned());
        } else if k == "state" {
            state = Some(v.into_owned());
        }
    }

    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err(AuthError::Callback(
            "Missing code or state in callback".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_parse_callback_query() {
        let path = "/callback?code=abc&state=xyz";
        let (code, state) = parse_callback_query(path).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn test_parse_callback_query_missing() {
        let path = "/callback?code=abc";
        assert!(parse_callback_query(path).is_err());
    }

    #[tokio::test]
    async fn refresh_sends_correct_form_body() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=my_refresh_tok"))
            .and(body_string_contains("client_id=my_client_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new_access",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        // ProviderOAuthConfig fields are &'static str; leak the dynamic URL for
        // the test (acceptable in test-only code).
        let token_url: &'static str = Box::leak(format!("{}/token", server.uri()).into_boxed_str());

        let config = ProviderOAuthConfig {
            name: "test",
            auth_url: "https://example.com/auth",
            token_url,
            scopes: &[],
            extra_auth_params: &[],
        };

        let token = refresh(&config, "my_client_id", None, "my_refresh_tok")
            .await
            .unwrap();

        assert_eq!(token.access_token, "new_access");
        // Refresh token should be preserved (response omitted it).
        assert_eq!(token.refresh_token.as_deref(), Some("my_refresh_tok"));
    }
}
