# Common development commands

- First setup: `mise trust`; `make toolchain-install`; `make deps-install`; on Linux install FFmpeg development libraries with `scripts/setup-ffmpeg-linux.sh` only if missing; `make build`.
- Root Makefile: `make help`; `make quality-check` runs the CI Rust/UI gates; `make audit` scans Cargo and production pnpm dependencies; `make outdated` checks direct Rust workspace and pnpm dependencies.
- Rust: `make format`, `make format-check`, `make rust-check`, `make clippy`, `make rust-test`; use direct Cargo commands for narrower package targets.
- UI: `pnpm --filter nian-ui dev|build|lint|typecheck|test`. Root aliases `pnpm dev|build|lint|typecheck|test` delegate to these.
- Desktop (requires display): `cargo run -p nian-desktop`; start UI dev server or build `ui/dist` first.
- Media worker: `cargo run -p nian-media-worker -- probe <file>`; `cargo run -p nian-media-worker -- run` for NDJSON IPC.
- Release helper checks: `pnpm run release:test`, `pnpm run media:gate`, `pnpm run release:version`.
- Repo instruction imports `/home/niand/.codex/RTK.md`: in this environment invoke shell commands through `rtk` (for example `rtk cargo test --workspace`).