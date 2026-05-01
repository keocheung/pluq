use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
