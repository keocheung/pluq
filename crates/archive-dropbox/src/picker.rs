//! Dropbox Chooser integration.
//!
//! Serves a localhost page that runs the Dropbox Chooser drop-in and maps the returned shared links
//! into source strings (stored verbatim — `is_dropbox_url` already accepts `preview` links). The
//! shared transport (serve page, open browser, capture POST) lives in `archive_auth::serve_picker_page`.

use archive_auth::{AuthError, PickedFile, serve_picker_page};

/// HTML page that runs the Dropbox Chooser. `__APP_KEY__` is substituted before serving. On success
/// it POSTs `[{ "link", "name", "bytes" }]` to `/picked`; on cancel it POSTs `[]`.
const CHOOSER_PAGE: &str = r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>pluq — pick from Dropbox</title></head>
<body style="font-family:sans-serif;text-align:center;margin-top:3rem">
<h2>Choose file(s) from Dropbox</h2>
<p>If the chooser doesn't open automatically, click the button (allow pop-ups for this page).</p>
<button id="choose-btn" style="font-size:1.1rem;padding:0.6rem 1.2rem">Choose from Dropbox</button>
<script src="https://www.dropbox.com/static/api/2/dropins.js" id="dropboxjs" data-app-key="__APP_KEY__"></script>
<script>
  function post(files) {
    fetch('/picked', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(files)
    }).then(function () {
      document.body.innerHTML = '<h2>Selection sent. You can close this tab.</h2>';
    });
  }
  function choose() {
    Dropbox.choose({
      linkType: 'preview',
      multiselect: true,
      success: function (files) {
        post((files || []).map(function (f) {
          return { link: f.link, name: f.name, bytes: f.bytes || 0 };
        }));
      },
      cancel: function () { post([]); }
    });
  }
  document.getElementById('choose-btn').addEventListener('click', choose);
  // Best-effort auto-open; if the pop-up is blocked the button remains for a manual gesture.
  window.addEventListener('load', function () { try { choose(); } catch (e) {} });
</script>
</body>
</html>
"#;

/// Run the Dropbox Chooser and return the picked files (empty = user cancelled).
///
/// Needs only the app key (the Chooser does its own browser auth). Reading the picked link later
/// goes through the normal dropbox auth path in `archive-source::resolve()`.
pub async fn pick_files(timeout_secs: u64) -> Result<Vec<PickedFile>, AuthError> {
    let (app_key, _secret) = crate::app_credentials().ok_or_else(|| {
        AuthError::OAuth("Dropbox app key not configured (PLUQ_DROPBOX_APP_KEY)".into())
    })?;

    let html = CHOOSER_PAGE.replace("__APP_KEY__", &app_key);
    let body = serve_picker_page(html, timeout_secs).await?;
    links_to_picked(&body)
}

/// Parse the Chooser POST body (`[{link,name,bytes}]`) into `PickedFile`s. Pure — unit-tested.
fn links_to_picked(body: &str) -> Result<Vec<PickedFile>, AuthError> {
    #[derive(serde::Deserialize)]
    struct ChosenLink {
        link: String,
        name: String,
        #[serde(default)]
        bytes: u64,
    }

    let chosen: Vec<ChosenLink> = serde_json::from_str(body)
        .map_err(|e| AuthError::OAuth(format!("could not parse Dropbox Chooser response: {e}")))?;

    Ok(chosen
        .into_iter()
        .map(|c| PickedFile {
            source: c.link,
            name: c.name,
            size: c.bytes,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_chooser_links_to_sources() {
        let body =
            r#"[{"link":"https://www.dropbox.com/s/abc/a.zip?dl=0","name":"a.zip","bytes":99}]"#;
        let picked = links_to_picked(body).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].source, "https://www.dropbox.com/s/abc/a.zip?dl=0");
        assert_eq!(picked[0].name, "a.zip");
        assert_eq!(picked[0].size, 99);
        // Produced source must be one `archive-source::resolve()` accepts.
        assert!(crate::is_dropbox_url(&picked[0].source));
    }

    #[test]
    fn accepts_scl_links() {
        let body = r#"[{"link":"https://www.dropbox.com/scl/fi/xyz/a.zip?rlkey=k","name":"a.zip","bytes":0}]"#;
        let picked = links_to_picked(body).unwrap();
        assert!(crate::is_dropbox_url(&picked[0].source));
    }

    #[test]
    fn empty_selection_is_empty_vec() {
        assert!(links_to_picked("[]").unwrap().is_empty());
    }

    #[test]
    fn bad_json_errors() {
        assert!(links_to_picked("nope").is_err());
    }

    #[test]
    fn page_template_has_placeholder() {
        assert!(CHOOSER_PAGE.contains("__APP_KEY__"));
    }
}
