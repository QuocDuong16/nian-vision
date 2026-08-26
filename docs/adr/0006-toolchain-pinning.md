# ADR-0006: Toolchain pinning and reproducible development

- Status: accepted
- Date: 2026-08-26

## Context

The master spec requires pinned toolchains and exact dependency versions.
Development machines manage runtimes with mise; CI (Forgejo Actions) runs
Docker-based jobs.

## Decision

* **Rust**: pinned to `1.98.0` (stable released 2026-08-20) via
  `rust-toolchain.toml` (with `rustfmt` + `clippy` components) — this file
  is authoritative for any direct cargo invocation. The project-level
  `.mise.toml` pins the same version for mise-managed machines.
* **Node**: `26.x` via `.mise.toml`; pnpm `11.22.0` recorded in the root
  `package.json` `packageManager` field.
* **Dependencies**: exact pins (`=x.y.z`) in `Cargo.toml` manifests for all
  direct dependencies, plus committed `Cargo.lock` and pnpm lockfile. No
  floating ranges, no `@latest` anywhere in automation.
* **FFmpeg**: version policy in ADR-0002; headers vendored with sha256
  verification.
* Edition 2024 across the workspace; `resolver = "3"`.

## Consequences

* `rustup` and mise may each hold a copy of the toolchain on dev machines;
  both resolve to the same version so behavior is identical.
* CI installs the pinned toolchain explicitly rather than using whatever the
  runner image ships.
