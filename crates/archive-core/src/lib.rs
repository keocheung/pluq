mod archive;
pub mod archive_reader;
pub mod codec;
mod entry;
mod error;
pub(crate) mod http;
pub mod junk;
pub(crate) mod local;
mod read_at;
pub(crate) mod source;
pub mod zip;

// Primary public API — use this.
pub use archive::Archive;

// Trait for format-agnostic archive operations.
pub use archive_reader::ArchiveReader;

// Shared codec types.
pub use codec::CompressionMethod;

// Data types.
pub use entry::ArchiveEntry;
pub use error::ArchiveError;

// Low-level building blocks (for power users / tests).
pub use http::HttpReadAt;
pub use local::LocalReadAt;
pub use read_at::ReadAt;
pub use source::Source;
pub use zip::ZipArchive;
pub use zip::ZipIndex;
