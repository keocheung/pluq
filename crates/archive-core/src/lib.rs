mod archive_error;
pub mod archive_reader;
pub mod codec;
mod entry;
mod facade;
pub mod io;
pub mod junk;
pub mod zip;

// Primary public API — use this (via archive-state in production code).
pub use facade::ArchiveCore;

// Trait for format-agnostic archive operations.
pub use archive_reader::ArchiveReader;

// Shared codec types.
pub use codec::CompressionMethod;

// Data types.
pub use archive_error::ArchiveError;
pub use entry::ArchiveEntry;

// I/O building blocks (for power users / tests).
pub use io::{HttpReadAt, LocalReadAt, ReadAt, Source};
pub use zip::ZipArchive;
pub use zip::ZipIndex;
