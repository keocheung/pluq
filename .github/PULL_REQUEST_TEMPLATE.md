## What & why

<!-- One paragraph: what this changes and the motivation. Link the issue if any. -->

## Checklist

- [ ] Conventional Commit title with crate scope (e.g. `fix(archive-core): …`)
- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test` passing
- [ ] Honors the 5-layer model + 6 Golden Rules (see `CONTEXT.md`)
- [ ] No new dependency that fails `cargo deny`
- [ ] Docs/CHANGELOG impact considered

## Notes for the reviewer

<!-- Anything tricky, trade-offs, follow-ups. -->
