# Task completion checks

- Follow `docs/testing.md`: select the narrowest relevant layer first; the project treats tests as part of the definition of done.
- Rust changes: `make format-check`, focused crate/test target, then relevant workspace checks; `make quality-check` runs workspace check, Windows-only Clippy, strict Clippy, and workspace tests with the UI gates.
- UI changes: run relevant UI tests, then `make ui-lint`, `make ui-typecheck`, `make ui-build`; the full UI suite is `make ui-test`.
- Release-script changes: `pnpm run release:test`; media/release contract changes may also require `pnpm run media:gate` and `pnpm run release:version`.
- Distinguish unit/fixture validation from real camera, OS, signed release, or device evidence. The manual recording smoke test is not run in CI and requires an explicitly chosen file or camera source.
- Prefer the narrowest quiet validation that proves the change; report checks actually run and any runtime/device checks not run.