# Archive Sniper — Post-v1 & Future Ideas

## Post-v1

- [ ] Source-aware default folder naming (use real GDrive/Cloud filename instead of URL-guessing).
- [ ] Parallel extraction for ZIP/non-solid RAR.
- [ ] Advanced cache invalidation (content hashing, detect modifications between opens).
- [ ] Configurable cache location (CLI flag or config file).
- [ ] Cache size limits / LRU eviction.
- [ ] Full range support detection with retry/fallback and user-facing guidance.
- [ ] OneDrive provider (`archive-onedrive` crate) + OAuth2.

---

## Future Ideas

*   **Write Support:** Archive creation (`snipe pack`). ZIP first, then 7z. TUI file browser for selecting files.
*   **Format Expansion:** 7z, TAR.GZ, ISO read support.
*   **SaaS Server:** HTTP API for web-based sniping. Metered/freemium. See `architecture/layer4-products.md`.
*   **KMP Apps:** Android/iOS/Desktop via Compose Multiplatform + UniFFI. See `architecture/layer4-products.md`.
*   **Cloud Archiving (Vault):** Stream files from cloud → compress → stream archive back.
*   **Plugin Hooks:** External tool integration (fzf, yazi) at search/select/browse decision points.
