# Toolchain and runtime

- Rust workspace, edition 2024. `mise.toml` is the authoritative toolchain pin; `rust-toolchain.toml` and `Cargo.toml` `rust-version` mirror it. Node, pnpm, and the Cargo helper tools are pinned there too, with verified artifacts in `mise.lock`.
- CI and release scripts resolve versions and archive checksums through `scripts/toolchain-pins.sh` and `scripts/toolchain-pins.ps1`; do not duplicate toolchain version literals in workflows or scripts.
- UI is React + TypeScript + Vite in `ui/`, pnpm workspace package name `nian-ui`.
- Desktop shell uses Tauri 2. Media and storage are Rust crates.
- SQLite uses `rusqlite` with bundled SQLite. FFmpeg runtime is 8.0.3 / ABI 62, dynamically loaded by the worker-side media implementation; headers/bindings are vendored/generated in the isolated low-level crates.
- Root `package.json` delegates UI scripts with `pnpm --filter nian-ui`; do not add a root `packageManager` field. Registry dependency versions are exactly pinned and lockfiles are committed. Internal path dependencies declare a semver major range in `[workspace.dependencies]` so cargo-deny can resolve inherited workspace dependencies; keep that range aligned with `workspace.package.version`.
- `tools/bindgen-gen` is an explicit-only utility requiring libclang; it is not a default workspace member.
