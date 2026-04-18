# Archive Sniper — Product Vision & Spec

## The Vision

Create the fastest, most surgical way to browse and extract files from cloud-hosted archives (ZIP/RAR5) without downloading the entire file.

### Target v1 Interface (Product Spec)

These commands define the CLI's public API. The TUI is the default entry point.

*   `snipe <url-or-path>` → **Interactive TUI browser** (The primary experience)
*   `snipe list <url-or-path>` → Non-interactive listing (for scripting)
*   `snipe get <url-or-path> [glob] [-o <dir>]` → Non-interactive extraction; no glob = extract all; `-o` sets output directory (default: `./<archive-stem>/`)
*   `snipe test <url-or-path>` → Verify archive integrity (extract nothing)
*   `snipe auth add/remove/list` → Manage OAuth tokens for Big 3 providers
*   `snipe feedback` → Open GitHub issues
