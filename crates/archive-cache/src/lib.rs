pub mod cache;
pub mod open;

pub use archive_core::FormatCacheState;
pub use cache::{cache_dir, cache_path_for, ArchiveCache, SCHEMA_VERSION};
pub use open::{open, Opened, PendingArchive};

pub mod reader;
pub use cache::CacheData;
pub type CachedArchive = archive::Archive<reader::MetadataReader<archive_source::ResolvedSource>>;
