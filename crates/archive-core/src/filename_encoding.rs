//! Filename decoding policy, independent of a particular source or cache.

/// `auto` uses ZIP metadata and detection; a named encoding overrides both.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilenameEncoding(Option<&'static encoding_rs::Encoding>);

impl FilenameEncoding {
    pub(crate) fn forced(self) -> Option<&'static encoding_rs::Encoding> {
        self.0
    }
}

impl std::str::FromStr for FilenameEncoding {
    type Err = String;

    fn from_str(label: &str) -> Result<Self, Self::Err> {
        let label = label.trim();
        if label.eq_ignore_ascii_case("auto") {
            return Ok(Self::default());
        }
        // Common Windows aliases absent from the WHATWG label table.
        let normalized = match label.to_ascii_lowercase().as_str() {
            "cp932" | "windows-932" => "shift_jis",
            "cp936" | "windows-936" => "gbk",
            _ => label,
        };
        let encoding = encoding_rs::Encoding::for_label(normalized.as_bytes())
            .filter(|e| *e != encoding_rs::REPLACEMENT)
            .ok_or_else(|| {
                format!("unsupported filename encoding '{label}' (try auto, cp932, gbk, or utf-8)")
            })?;
        Ok(Self(Some(encoding)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_aliases_and_rejects_unknown_encodings() {
        for label in ["cp932", "CP932", "shift_jis", "Shift-JIS", "windows-932"] {
            assert_eq!(
                label.parse::<FilenameEncoding>().unwrap().forced(),
                Some(encoding_rs::SHIFT_JIS)
            );
        }
        assert!(
            "auto"
                .parse::<FilenameEncoding>()
                .unwrap()
                .forced()
                .is_none()
        );
        assert_eq!(
            "gbk".parse::<FilenameEncoding>().unwrap().forced(),
            Some(encoding_rs::GBK)
        );
        assert_eq!(
            "utf-8".parse::<FilenameEncoding>().unwrap().forced(),
            Some(encoding_rs::UTF_8)
        );
        assert!("nonsense".parse::<FilenameEncoding>().is_err());
        assert!("replacement".parse::<FilenameEncoding>().is_err());
    }
}
