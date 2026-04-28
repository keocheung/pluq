pub mod archive_reader;
pub mod codec;
mod core_error;
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
pub use archive_reader::{ArchiveReader, BatchResult, extract_selected_default};

// Shared codec types.
pub use codec::CompressionMethod;

// Data types.
pub mod limits;
pub mod path_safety;
pub use core_error::{CoreError, PasswordDetectionMethod};
pub use entry::ArchiveEntry;
pub use limits::{CodecConfig, LimitMode, RatioLimit, ResourceLimits};
pub use path_safety::{PathDecision, PathPolicy, evaluate_path};

// Progress reporting + cancellation.
pub use progress::ProgressReporter;

// Scan state.
pub use scan::ScanInfo;

// I/O building blocks (for power users / tests).
pub use io::{CachingReadAt, HttpReadAt, LocalPath, LocalReadAt, ReadAt, ReadError};
pub use rar::{RAR4_SIGNATURE, RAR5_SIGNATURE, RarArchive, RarEntryLocation};
pub use rar4::{Rar4Archive, Rar4EntryLocation};
pub use zip::{AesExtraInfo, ZipArchive, ZipEntryLocation, ZipIndex};
