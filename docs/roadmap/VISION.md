# pluq — Product Vision & Spec

## The Vision

Create the fastest, most surgical way to browse and extract files from cloud-hosted archives (ZIP/RAR5) without downloading the entire file.

### Target v1 Interface (Product Spec)

These commands define the CLI's public API. The TUI is the default entry point.

*   `pluq <url-or-path>` → **Interactive TUI browser** (The primary experience)
*   `pluq list <url-or-path>` → Non-interactive listing (for scripting)
*   `pluq get <url-or-path> [glob] [-o <dir>]` → Non-interactive extraction; no glob = extract all; `-o` sets output directory (default: `./<archive-stem>/`)
*   `pluq test <url-or-path>` → Verify archive integrity (extract nothing)
*   `pluq auth add/remove/list` → Manage OAuth tokens for Big 3 providers
*   `pluq feedback` → Open GitHub issues
