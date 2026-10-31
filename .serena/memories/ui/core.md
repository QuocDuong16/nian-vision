# UI module boundary

- React + TypeScript + Vite lives in `ui/`; Tauri commands are the typed bridge to native services.
- The UI presents camera, recording, playback/live-view, ONVIF, PTZ, and event-review workflows; backend capability/status and opaque session handles govern available actions.
- Keep credentials, resolved stream URLs, native filesystem paths, keyring access, and FFmpeg authority in native/Rust layers. UI should receive only safe DTOs and display-ready state.
- Lifecycle and async UI state must handle cancellation, stale generations, RPC failures, and backend reconnect/unavailable states explicitly.
- Run frontend validation through pnpm workspace package `nian-ui`; see `docs/development.md` and `docs/testing.md`.
- TypeScript tooling is split: `ui` devDependencies alias `@typescript/native` to `npm:typescript@7.0.2` (its `tsc` binary runs `pnpm typecheck`) and alias `typescript` to `npm:@typescript/typescript6@6.0.2` so `typescript-eslint` 8.x resolves a compiler inside its `<6.1.0` peer range (`tsc6`). Keep both aliases together when upgrading TypeScript.