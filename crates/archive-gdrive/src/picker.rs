//! Google Picker integration.
//!
//! Serves a localhost page that runs the Google Picker with a pre-supplied OAuth token, and maps the
//! picked file IDs into `drive.google.com/open?id=…` source strings. The shared transport (serve page,
//! open browser, capture POST) lives in `archive_auth::serve_picker_page`.

use std::sync::Arc;

use archive_auth::{
    AuthError, GDRIVE_CONFIG, PickedFile, StoredToken, TokenStore, refresh, serve_picker_page,
};

/// HTML page that loads the Google Picker. Placeholders are substituted before serving:
/// `__OAUTH_TOKEN__`, `__DEVELOPER_KEY__`, `__APP_ID__`. On pick it POSTs the selection as
/// `[{ "id", "name", "size" }]` to `/picked`; on cancel it POSTs `[]`.
const PICKER_PAGE: &str = r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>pluq — pick from Google Drive</title></head>
<body style="font-family:sans-serif;text-align:center;margin-top:3rem">
<h2>Opening Google Drive picker…</h2>
<p>If nothing appears, allow pop-ups for this page.</p>
<script src="https://apis.google.com/js/api.js"></script>
<script>
  function onApiLoad() { gapi.load('picker', createPicker); }
  function post(files) {
    fetch('/picked', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(files)
    }).then(function () {
      document.body.innerHTML = '<h2>Selection sent. You can close this tab.</h2>';
    });
  }
  function createPicker() {
    // Restrict to the user's own files under My Drive, matching what they see in the
    // regular Drive UI — the default DOCS view also surfaces orphaned / other-app files
    // (the "com.xxx" app folders) that aren't in My Drive. Levers if this needs tuning:
    //   .setOwnedByMe(false) — also show "Shared with me" archives
    //   .setMimeTypes('application/zip,application/x-rar-compressed,application/vnd.rar')
    //       — hard archive-only filter (risky: Drive stores some .rar as octet-stream)
    var view = new google.picker.DocsView(google.picker.ViewId.DOCS)
      .setParent('root')
      .setOwnedByMe(true)
      .setIncludeFolders(true)
      .setSelectFolderEnabled(false);
    var picker = new google.picker.PickerBuilder()
      .setOAuthToken('__OAUTH_TOKEN__')
      .setDeveloperKey('__DEVELOPER_KEY__')
      .setAppId('__APP_ID__')
      .addView(view)
      .enableFeature(google.picker.Feature.MULTISELECT_ENABLED)
      .setCallback(function (data) {
        if (data.action === google.picker.Action.PICKED) {
          var files = (data.docs || []).map(function (d) {
            return { id: d.id, name: d.name, size: Number(d.sizeBytes) || 0 };
          });
          post(files);
        } else if (data.action === google.picker.Action.CANCEL) {
          post([]);
        }
      })
      .build();
    picker.setVisible(true);
  }
  onApiLoad();
</script>
</body>
</html>
"#;

/// Run the Google Picker and return the picked files (empty = user cancelled).
///
/// Requires a stored `gdrive` token (caller ensures interactive auth first; this only refreshes a
/// stale one silently), the OAuth client id (for refresh + appId), and the API key (developer key).
pub async fn pick_files(
    store: Arc<TokenStore>,
    timeout_secs: u64,
) -> Result<Vec<PickedFile>, AuthError> {
    let token = store.get("gdrive").ok_or_else(|| {
        AuthError::OAuth("not authenticated with Google Drive — run `pluq auth add gdrive`".into())
    })?;
    let client_id = crate::oauth_client_id().ok_or_else(|| {
        AuthError::OAuth(
            "Google Drive OAuth client ID not configured (PLUQ_GDRIVE_CLIENT_ID)".into(),
        )
    })?;
    let client_secret = crate::oauth_client_secret();
    let api_key = crate::api_key().ok_or_else(|| {
        AuthError::OAuth("Google Drive API key not configured (PLUQ_GDRIVE_KEY)".into())
    })?;

    let token = ensure_fresh(token, &store, &client_id, client_secret.as_deref()).await?;

    // appId = GCP project number = the prefix of the client_id before the first '-'.
    let app_id = client_id.split('-').next().unwrap_or_default().to_string();

    let html = PICKER_PAGE
        .replace("__OAUTH_TOKEN__", &token.access_token)
        .replace("__DEVELOPER_KEY__", &api_key)
        .replace("__APP_ID__", &app_id);

    let body = serve_picker_page(html, timeout_secs).await?;
    docs_to_picked(&body)
}

/// Refresh the token if it is expired (or within 60s of expiry) and a refresh token exists.
/// Persists the refreshed token. Returns the token to use.
async fn ensure_fresh(
    token: StoredToken,
    store: &TokenStore,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<StoredToken, AuthError> {
    if let Some(refresh_token) = token
        .refresh_token
        .as_ref()
        .filter(|_| token.is_expired(60))
    {
        let fresh = refresh(&GDRIVE_CONFIG, client_id, client_secret, refresh_token).await?;
        let _ = store.save("gdrive", fresh.clone());
        return Ok(fresh);
    }
    Ok(token)
}

/// Parse the picker POST body (`[{id,name,size}]`) into source-string `PickedFile`s.
/// Pure — unit-tested without a browser.
fn docs_to_picked(body: &str) -> Result<Vec<PickedFile>, AuthError> {
    #[derive(serde::Deserialize)]
    struct PickedDoc {
        id: String,
        name: String,
        #[serde(default)]
        size: u64,
    }

    let docs: Vec<PickedDoc> = serde_json::from_str(body)
        .map_err(|e| AuthError::OAuth(format!("could not parse Google Picker response: {e}")))?;

    Ok(docs
        .into_iter()
        .map(|d| PickedFile {
            source: format!("https://drive.google.com/open?id={}", d.id),
            name: d.name,
            size: d.size,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_docs_to_open_id_urls() {
        let body = r#"[{"id":"FILEID1","name":"a.zip","size":42},{"id":"FILEID2","name":"b.rar","size":0}]"#;
        let picked = docs_to_picked(body).unwrap();
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].source, "https://drive.google.com/open?id=FILEID1");
        assert_eq!(picked[0].name, "a.zip");
        assert_eq!(picked[0].size, 42);
        assert_eq!(picked[1].source, "https://drive.google.com/open?id=FILEID2");

        // The produced source must be one `archive-source::resolve()` accepts.
        assert!(crate::is_gdrive_url(&picked[0].source));
        assert_eq!(
            crate::extract_file_id(&picked[0].source).as_deref(),
            Some("FILEID1")
        );
    }

    #[test]
    fn empty_selection_is_empty_vec() {
        assert!(docs_to_picked("[]").unwrap().is_empty());
    }

    #[test]
    fn bad_json_errors() {
        assert!(docs_to_picked("not json").is_err());
    }

    #[test]
    fn page_template_has_all_placeholders() {
        // Guard against typos that would leave a placeholder unsubstituted at runtime.
        assert!(PICKER_PAGE.contains("__OAUTH_TOKEN__"));
        assert!(PICKER_PAGE.contains("__DEVELOPER_KEY__"));
        assert!(PICKER_PAGE.contains("__APP_ID__"));
    }
}
