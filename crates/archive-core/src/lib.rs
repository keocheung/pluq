mod core_error;
pub mod archive_reader;
pub mod codec;
mod entry;
mod facade;
pub mod io;
pub mod junk;
mod progress;
pub mod rar;
pub mod zip;

// Primary public API — use this (via archive crate in production code).
pub use facade::ArchiveCore;

// Trait for format-agnostic archive operations.
pub use archive_reader::ArchiveReader;

// Shared codec types.
pub use codec::CompressionMethod;

// Data types.
pub use core_error::{CoreError, PasswordDetectionMethod};
pub use entry::ArchiveEntry;

// Progress reporting + cancellation.
pub use progress::ProgressReporter;

// I/O building blocks (for power users / tests).
pub use io::{HttpReadAt, LocalReadAt, ReadAt, ReadError, Source};
pub use zip::ZipArchive;
pub use zip::ZipIndex;
