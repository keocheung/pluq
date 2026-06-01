# Security Policy

## Supported versions

pluq is pre-1.0; only the latest released version receives security fixes.

| Version | Supported |
|---------|-----------|
| 0.1.x   | ✅        |
| < 0.1   | ❌        |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub Security Advisories:
<https://github.com/stefandekanski/pluq/security/advisories/new>

If you cannot use GitHub Advisories, email **stefandekanski@gmail.com** with subject
`pluq security`.

Please include: affected version (`pluq --version`), OS + arch, a description, and reproduction
steps or a proof-of-concept archive if relevant. **Do not attach archives containing real
private data or credentials.**

You can expect an acknowledgement within a few days. As a solo-maintained project, fixes are
best-effort; coordinated disclosure timelines will be agreed case by case.

## Scope notes

pluq parses untrusted archives and fetches from untrusted URLs. Anti-bomb resource limits, path
traversal guards, symlink-escape checks, and an LZMA dictionary cap are in place
(see `docs/ROADMAP.md` → completed security hardening). Reports that strengthen these are welcome.
