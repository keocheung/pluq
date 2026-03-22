//! RAR4 format support.
//!
//! Custom header parser for listing (enables cloud sniping via range requests).
//! Extraction delegates to `unrar` — local shortcut or synthetic archive for cloud.

pub(crate) mod parse;
