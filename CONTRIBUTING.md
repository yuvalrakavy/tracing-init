# Contributing to tracing-init

Thanks for taking the time! This document is short on purpose.

## Before you start

- For non-trivial changes — new destinations, breaking API changes,
  configuration model changes — please open an issue first to agree on the
  shape of the change. Small fixes can go straight to a PR.
- The crate's scope is "stand up a `tracing` subscriber for real-world Rust
  services, with a small, predictable surface". New features should fit that
  framing; if you find yourself adding a new logging facade or a custom format
  language, it probably belongs in a separate crate.

## Building and testing

```bash
# Default features (config + file + gelf)
cargo test

# Everything, including OpenTelemetry
cargo test --all-features

# Minimal build (no default features)
cargo check --no-default-features
```

Please run `cargo fmt` and `cargo clippy --all-features` before submitting.

## Pull request checklist

- [ ] `cargo test --all-features` passes.
- [ ] `cargo clippy --all-features -- -D warnings` is clean.
- [ ] Public items are documented with `///` doc comments.
- [ ] New behavior is covered by a test in `src/tests/`.
- [ ] User-visible changes are noted in `CHANGELOG.md` under `[Unreleased]`.
- [ ] One logical change per PR. Refactors and feature work go in separate
      PRs whenever practical.

## Reporting bugs

When filing an issue, please include:

- The version of `tracing-init` (and feature flags you enabled).
- A minimal reproduction (a `logging.toml` plus a few lines of Rust is ideal).
- What you expected to happen and what actually happened.
- For OTel issues, the collector you're using and its endpoint.

## Code of conduct

Be kind, assume good faith, and keep discussion on the technical substance.
