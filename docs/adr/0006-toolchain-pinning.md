# ADR-0006: Toolchain pinning and reproducible development

- Status: accepted
- Date: 2026-08-26

## Context

The master spec requires pinned toolchains and exact dependency versions.
Development machines manage runtimes with mise; CI (Forgejo Actions) runs
Docker-based jobs.

## Decision

* **Rust**: `mise.toml` is authoritative. `rust-toolchain.toml` mirrors the
  pinned channel (with `rustfmt` + `clippy`) for direct cargo/rustup
  invocations, and `Cargo.toml` `rust-version` mirrors the same pin.
* **Node and pnpm**: exact versions in `mise.toml`; `mise.lock` records
  resolved artifacts for supported platforms. CI and release workflows resolve
  versions and archive checksums from these files through
  `scripts/toolchain-pins.sh` and `scripts/toolchain-pins.ps1` instead of
  duplicating literals.
  pnpm uses its native GitHub release binaries (including macOS x64).
  Do not add a `packageManager` field that makes pnpm switch versions.
* **Dependencies**: exact pins (`=x.y.z`) in `Cargo.toml` manifests for all
  direct dependencies, plus committed `Cargo.lock` and pnpm lockfile. No
  floating ranges, no `@latest` anywhere in automation.
* **Cargo checks**: `cargo-audit`, `cargo-deny`, `cargo-llvm-cov`, and
  `cargo-machete` are pinned in `mise.toml` and resolved by `mise.lock`; use the
  root Makefile for the local quality, audit, and coverage commands.
  `cargo-outdated` is intentionally not pinned: it misses
  `[workspace.dependencies]` inheritance, so `make rust-outdated` runs
  `scripts/run_cargo_outdated.mjs` against `cargo metadata` and the crates.io
  sparse index instead.
* **FFmpeg**: version policy in ADR-0002; headers vendored with sha256
  verification.
* Edition 2024 across the workspace; `resolver = "3"`.

## Consequences

* `rustup` and mise may each hold a copy of the toolchain on dev machines;
  both resolve to the same version so behavior is identical.
* CI installs the pinned toolchain from the `mise.toml` pin rather than using
  whatever the runner image ships.
