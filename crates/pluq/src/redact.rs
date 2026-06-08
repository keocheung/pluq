use regex::Regex;
use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::OnceLock;

use tracing_appender::non_blocking::NonBlocking;
use tracing_subscriber::fmt::MakeWriter;

pub fn scrub(input: &str) -> Cow<'_, str> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();

    let url_re = URL_RE.get_or_init(|| Regex::new(r"https?://[^\s]+").unwrap());

    let token_re = TOKEN_RE.get_or_init(|| {
        Regex::new(r#"(?i)(authorization|bearer|access_token|refresh_token|token|api[_-]?key|client[_-]?secret)(["':= ]+)[^\s"',&]+"#).unwrap()
    });

    let mut result = Cow::Borrowed(input);
    if url_re.is_match(&result) {
        result = Cow::Owned(url_re.replace_all(&result, "<url>").into_owned());
    }
    if token_re.is_match(&result) {
        result = Cow::Owned(
            token_re
                .replace_all(&result, "${1}${2}<redacted>")
                .into_owned(),
        );
    }
    result
}

fn collapse_homes(input: &str, real_home: Option<&str>) -> String {
    static USER_HOME_RE: OnceLock<Regex> = OnceLock::new();

    let mut s = scrub(input).into_owned();

    if let Some(home_str) = real_home
        && home_str.len() > 1
    {
        s = s.replace(home_str, "~");
    }

    // /Users/<name>, /home/<name>, C:\Users\<name>  ->  ~
    let re = USER_HOME_RE
        .get_or_init(|| Regex::new(r"(?i)(?:/Users/|/home/|[A-Z]:\\Users\\)[^/\\]+").unwrap());
    re.replace_all(&s, "~").into_owned()
}

/// Stronger scrub for diagnostic dumps (bug reports, crash reports).
///
/// Runs the standard URL + token `scrub`, then collapses home directories to `~`:
/// the current user's real home first (most precise), then generic per-OS user-home
/// prefixes so paths belonging to *other* users are anonymised too.
pub fn scrub_report(input: &str) -> String {
    let home_dir = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf());
    let real_home = home_dir.as_deref().and_then(|p| p.to_str());
    collapse_homes(input, real_home)
}

/// A writer that scrubs everything it receives through [`scrub`] before
/// forwarding to the inner writer.
///
/// It buffers all bytes written for one event and scrubs the **whole** buffer
/// on flush/drop — never per `write` call — so a secret (e.g. a Dropbox
/// `rlkey`) that the formatter splits across multiple `write` calls can never
/// slip through un-scrubbed.
pub struct RedactWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> RedactWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buf);
        let scrubbed = scrub(&text);
        let res = self.inner.write_all(scrubbed.as_bytes());
        self.buf.clear();
        res
    }
}

impl<W: Write> Write for RedactWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactWriter<W> {
    fn drop(&mut self) {
        // Emit any buffered event even if the formatter never called flush.
        let _ = self.emit();
    }
}

/// Wraps a [`NonBlocking`] appender so the fmt subscriber's formatted output
/// passes through [`RedactWriter`] before reaching the log file. Install via
/// `tracing_subscriber::fmt().with_writer(RedactMakeWriter::new(non_blocking))`.
#[derive(Clone)]
pub struct RedactMakeWriter {
    inner: NonBlocking,
}

impl RedactMakeWriter {
    pub fn new(inner: NonBlocking) -> Self {
        Self { inner }
    }
}

impl<'a> MakeWriter<'a> for RedactMakeWriter {
    type Writer = RedactWriter<NonBlocking>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactWriter::new(self.inner.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_scrub_url() {
        assert_eq!(
            scrub("opening https://example.com/file.zip now"),
            "opening <url> now"
        );
    }

    #[test]
    fn test_scrub_token() {
        assert_eq!(scrub("Bearer xyz123"), "Bearer <redacted>");
        assert_eq!(scrub("access_token=\"abc\""), "access_token=\"<redacted>\"");
    }

    #[test]
    fn test_scrub_report_unix_home() {
        let out = scrub_report("opening /home/someoneelse/data/secret.zip");
        assert!(out.contains("~/data/secret.zip"), "got: {out}");
        assert!(!out.contains("someoneelse"), "got: {out}");
    }

    #[test]
    fn test_scrub_report_macos_home() {
        let out = scrub_report("/Users/jdoe/Documents/a.rar");
        assert!(!out.contains("jdoe"), "got: {out}");
        assert!(out.contains('~'), "got: {out}");
    }

    #[test]
    fn test_scrub_report_windows_home() {
        let out = scrub_report(r"C:\Users\jdoe\stuff");
        assert!(!out.contains("jdoe"), "got: {out}");
    }

    #[test]
    fn test_scrub_report_still_redacts_urls_and_tokens() {
        let out = scrub_report("GET https://drive.google.com/x Bearer abc123");
        assert!(out.contains("<url>"), "got: {out}");
        assert!(out.contains("<redacted>"), "got: {out}");
    }

    #[test]
    fn test_collapse_homes_root_safety() {
        let out = collapse_homes("opening /var/log/foo", Some("/"));
        assert_eq!(out, "opening /var/log/foo");
    }

    #[test]
    fn test_collapse_homes_real_home() {
        let out = collapse_homes("file in /Users/jdoe/stuff", Some("/Users/jdoe"));
        assert_eq!(out, "file in ~/stuff");
    }

    #[test]
    fn test_collapse_homes_windows_space() {
        let out = collapse_homes(r"C:\Users\John Doe\x", None);
        assert_eq!(out, r"~\x");
    }

    #[test]
    fn redact_writer_scrubs_dropbox_rlkey_url() {
        let mut sink = Vec::new();
        {
            let mut w = RedactWriter::new(&mut sink);
            write!(
                w,
                "url=https://www.dropbox.com/scl/fi/abc/file.zip?rlkey=SECRET123&dl=0 next=1"
            )
            .unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(out.contains("<url>"), "got: {out}");
        assert!(!out.contains("rlkey"), "rlkey leaked: {out}");
        assert!(!out.contains("SECRET123"), "secret leaked: {out}");
    }

    #[test]
    fn redact_writer_scrubs_url_split_across_writes() {
        // The formatter may emit one event in several write() calls; a secret URL
        // split mid-token across calls must STILL be scrubbed. This proves the
        // buffer-then-scrub-on-emit design (the security-critical property).
        let mut sink = Vec::new();
        {
            let mut w = RedactWriter::new(&mut sink);
            w.write_all(b"opening https://www.dropbox.com/scl/fi/")
                .unwrap();
            w.write_all(b"abc/file.zip?rlkey=SECRET").unwrap();
            w.write_all(b"123&dl=0 done").unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(
            !out.contains("rlkey"),
            "rlkey leaked across chunk boundary: {out}"
        );
        assert!(
            !out.contains("SECRET123"),
            "secret leaked across chunk boundary: {out}"
        );
        assert!(out.contains("<url>"), "got: {out}");
    }

    #[test]
    fn redact_writer_scrubs_access_token() {
        let mut sink = Vec::new();
        {
            let mut w = RedactWriter::new(&mut sink);
            write!(w, "access_token=abcdef123456").unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(out.contains("<redacted>"), "got: {out}");
        assert!(!out.contains("abcdef123456"), "token leaked: {out}");
    }

    #[test]
    fn redact_writer_passes_clean_line_through_unchanged() {
        let mut sink = Vec::new();
        {
            let mut w = RedactWriter::new(&mut sink);
            write!(w, "scan complete: 12 entries in archive").unwrap();
            w.flush().unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert_eq!(out, "scan complete: 12 entries in archive");
    }

    #[test]
    fn redact_writer_emits_on_drop_without_flush() {
        // No explicit flush: Drop must still emit the scrubbed buffer.
        let mut sink = Vec::new();
        {
            let mut w = RedactWriter::new(&mut sink);
            write!(w, "url=https://x/y?rlkey=ZZZ end").unwrap();
        }
        let out = String::from_utf8(sink).unwrap();
        assert!(out.contains("<url>"), "drop did not emit: {out}");
        assert!(!out.contains("rlkey"), "rlkey leaked on drop path: {out}");
    }
}
