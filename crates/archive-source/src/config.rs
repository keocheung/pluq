/// Configuration for source resolution.
///
/// Product layers construct this at startup and pass it to `resolve()`.
///
/// Phase 4: minimal (no OAuth). Phase 4.5: adds `token_store` for OAuth tokens.
/// The `resolve()` signature won't change — just richer config.
#[derive(Debug, Default)]
pub struct SourceConfig {
    // Phase 4.5 will add:  pub token_store: Option<TokenStore>
    // For now, this is intentionally empty. resolve() uses it for future extensibility.
}

impl SourceConfig {
    pub fn new() -> Self {
        Self::default()
    }
}
