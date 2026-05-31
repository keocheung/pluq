# Privacy

pluq collects **zero telemetry**. It never phones home, never sends usage data, and has no
analytics of any kind.

## Crash & bug reports stay on your machine

- `pluq bug-report` writes a **local file** (`pluq-bugreport-<timestamp>.txt`) in your current
  directory. It is **never uploaded**. You choose whether to attach it to an issue.
- If pluq crashes, a crash report is written to a **local temporary file**. Same rule: nothing
  leaves your machine unless you send it.
- Before writing, pluq redacts URLs, tokens/credentials, and home-directory paths from the
  report. Review the plaintext file before sharing — it is yours to inspect and edit.

## What this means

Because pluq transmits nothing on its own, there is no data collection to consent to. The only
moment data leaves your machine is when **you** decide to attach a file to a public issue.
A public issue is public forever — remove anything sensitive first.

## Your credentials

OAuth tokens for Google Drive / Dropbox are stored locally by your OS keyring. `PLUQ_*`
environment variables (bring-your-own credentials) are read at runtime and never written to
reports — only their **names** appear in diagnostics, never their values.
