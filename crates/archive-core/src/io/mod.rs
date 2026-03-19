pub(crate) mod http_reader;
mod local_reader;
mod read_at;
pub mod read_error;
pub(crate) mod source;

pub use http_reader::HttpReadAt;
pub use local_reader::LocalReadAt;
pub use read_at::ReadAt;
pub use read_error::*;
pub use source::Source;

use std::path::Path;

/// Extension trait for `ReadAt` sources that may be backed by a local file.
///
/// Implemented for all concrete `ReadAt` types.  [`LocalReadAt`] returns
/// `Some(&self.path)`; [`HttpReadAt`] and any other remote source returns `None`.
///
/// Used by [`RarArchive::open`](crate::rar::RarArchive::open) to capture the local
/// path at construction time, enabling the extraction shortcut that passes the path
/// directly to `unrar` instead of building a synthetic in-memory archive.
pub(crate) trait LocalPath {
    fn local_path(&self) -> Option<&Path>;
}

impl LocalPath for LocalReadAt {
    fn local_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

impl LocalPath for HttpReadAt {
    fn local_path(&self) -> Option<&Path> {
        None
    }
}

impl LocalPath for Source {
    fn local_path(&self) -> Option<&Path> {
        Source::local_path(self)
    }
}
