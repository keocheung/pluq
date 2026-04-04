mod classify;
mod config;
mod error;
mod resolve;
mod resolved;

pub use classify::{classify_transport, ProviderError};
pub use config::SourceConfig;
pub use error::SourceError;
pub use resolve::resolve;
pub use resolved::ResolvedSource;
