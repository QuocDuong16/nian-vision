# Toolchain and runtime

- Rust workspace, edition 2024; pinned Rust 1.98.1 in `rust-toolchain.toml` and `mise.toml`.
- Node 26.9.0 and pnpm 12.7.0 are pinned by mise; UI is React + TypeScript + Vite in `ui/`, pnpm workspace package name `nian-ui`.
- Desktop shell uses Tauri 2. Media and storage are Rust crates.
- SQLite uses `rusqlite` with bundled SQLite. FFmpeg runtime is 8.0.3 / ABI 62, dynamically loaded by the worker-side media implementation; headers/bindings are vendored/generated in the isolated low-level crates.
- Root `package.json` delegates UI scripts with `pnpm --filter nian-ui`; do not add a root `packageManager` field. Dependency versions are exactly pinned and lockfiles are committed.
- `tools/bindgen-gen` is an explicit-only utility requiring libclang; it is not a default workspace member.
