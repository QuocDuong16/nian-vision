# Common development commands

- First setup: `mise trust`; `mise install --locked rust node pnpm`; `pnpm install`; on Linux install FFmpeg development libraries with `scripts/setup-ffmpeg-linux.sh` only if missing; `cargo build`.
- Rust: `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings`; `cargo test --workspace`.
- UI: `pnpm --filter nian-ui dev|build|lint|typecheck|test`. Root aliases `pnpm dev|build|lint|typecheck|test` delegate to these.
- Desktop (requires display): `cargo run -p nian-desktop`; start UI dev server or build `ui/dist` first.
- Media worker: `cargo run -p nian-media-worker -- probe <file>`; `cargo run -p nian-media-worker -- run` for NDJSON IPC.
- Release helper checks: `pnpm run release:test`, `pnpm run media:gate`, `pnpm run release:version`.
- Repo instruction imports `/home/niand/.codex/RTK.md`: in this environment invoke shell commands through `rtk` (for example `rtk cargo test --workspace`).