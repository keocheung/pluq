use std::sync::Arc;

use archive_auth::TokenStore;

/// Configuration for source resolution.
///
/// Product layers construct this at startup and pass it to `resolve()`.
#[derive(Default)]
pub struct SourceConfig {
    /// Optional OAuth token store. When present, `resolve()` will prefer OAuth
    /// credentials over the app-level fallback (API key / app credentials).
    ///
    /// `None` → resolve() uses only app-level credentials (phase 4 behaviour).
    pub token_store: Option<Arc<TokenStore>>,
}

impl SourceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style setter. Use when constructing SourceConfig at product startup.
    pub fn with_token_store(mut self, store: Arc<TokenStore>) -> Self {
        self.token_store = Some(store);
        self
    }
}
