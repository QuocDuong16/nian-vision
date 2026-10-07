# Toolchain and runtime

- Rust workspace, edition 2024; pinned Rust 1.99.0 in `rust-toolchain.toml` and `mise.toml`.
- Node and pnpm are pinned by mise; UI is React + TypeScript + Vite in `ui/`, pnpm workspace package name `nian-ui`.
- Desktop shell uses Tauri 2. Media and storage are Rust crates.
- SQLite uses `rusqlite` with bundled SQLite. FFmpeg runtime is 8.0.3 / ABI 62, dynamically loaded by the worker-side media implementation; headers/bindings are vendored/generated in the isolated low-level crates.
- Root `package.json` delegates UI scripts with `pnpm --filter nian-ui`; do not add a root `packageManager` field. Registry dependency versions are exactly pinned and lockfiles are committed. Internal path dependencies declare a semver major range in `[workspace.dependencies]` so cargo-deny can resolve inherited workspace dependencies; keep that range aligned with `workspace.package.version`.
- `tools/bindgen-gen` is an explicit-only utility requiring libclang; it is not a default workspace member.
