# Common development commands

- First setup: `mise trust`; `make toolchain-install`; `make deps-install`; on Linux install FFmpeg development libraries with `scripts/setup-ffmpeg-linux.sh` only if missing; `make build`.
- Root Makefile: `make help`; `make toolchain-check` verifies the active tools against `mise.toml`; `make quality-check` runs the CI Rust/UI gates plus tooling-script tests; `make audit` scans Cargo and production pnpm dependencies; `make outdated` checks direct Rust dependencies through `scripts/run_cargo_outdated.mjs` (cargo metadata plus crates.io sparse index, including `[workspace.dependencies]` inheritance) and every pnpm workspace package recursively; `make scripts-test` runs the tooling-script unit tests; `make rust-machete` checks unused Rust dependencies; `make coverage` generates an HTML coverage report.
- Rust: `make format`, `make format-check`, `make rust-check`, `make clippy`, `make rust-test`; use direct Cargo commands for narrower package targets.
- UI: `pnpm --filter nian-ui dev|build|lint|typecheck|test`. Root aliases `pnpm dev|build|lint|typecheck|test` delegate to these.
- Desktop (requires display): `cargo run -p nian-desktop`; start UI dev server or build `ui/dist` first.
- Media worker: `cargo run -p nian-media-worker -- probe <file>`; `cargo run -p nian-media-worker -- run` for NDJSON IPC.
- Release helper checks: `pnpm run release:test`, `pnpm run scripts:test`, `pnpm run media:gate`, `pnpm run release:version`.
- Repo instruction imports `/home/niand/.codex/RTK.md`: in this environment invoke shell commands through `rtk` (for example `rtk cargo test --workspace`).
- `make clean` removes the Cargo target directory and generated `ui/dist` output.