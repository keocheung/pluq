//! RAR format support.
//!
//! v1 scope: RAR5 non-solid archives (listing + extraction).
//! RAR4 archives are detected and rejected with a clear error.

pub(crate) mod parse;
