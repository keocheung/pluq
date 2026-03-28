mod core_error;
pub mod archive_reader;
pub mod codec;
mod entry;
pub mod io;
pub mod junk;
mod progress;
pub mod rar;
pub mod rar4;
mod scan;
mod time;
pub mod zip;

// Trait for format-agnostic archive operations.
pub use archive_reader::ArchiveReader;

// Shared codec types.
pub use codec::CompressionMethod;

// Data types.
pub use core_error::{CoreError, PasswordDetectionMethod};
pub use entry::ArchiveEntry;

// Progress reporting + cancellation.
pub use progress::ProgressReporter;

// Scan state.
pub use scan::ScanInfo;

// I/O building blocks (for power users / tests).
pub use io::{CachingReadAt, HttpReadAt, LocalReadAt, ReadAt, ReadError, Source};
pub use rar::{RarArchive, RarEntryLocation, RAR4_SIGNATURE, RAR5_SIGNATURE};
pub use rar4::{Rar4Archive, Rar4EntryLocation};
pub use zip::{AesExtraInfo, ZipArchive, ZipEntryLocation, ZipIndex};
