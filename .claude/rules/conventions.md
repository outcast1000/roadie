# Conventions

Always loaded. The path-scoped files add detail; these apply everywhere.

## Errors and feedback

- Rust functions that can fail return `Result<T, String>` with a message a user could read:
  name the file, port or tool (`"create {path}: {e}"`), never a bare `e.to_string()` of a
  reqwest error — use `recipe::httpsteps::err_chain` so the cause survives.
- Every `catch` in TypeScript logs with `console.error("Failed to <action>:", e)`; UI code then
  shows the failure (a callout on the tool card, `errors[name]` in `useTools`), never swallows it.
- Long operations report progress: install goes through `Progress` → `tool-install-progress`
  events and, for API-originated installs, `requests::set_progress`.
- A tool's log tail is the diagnostic surface. Anything that decides "it failed to start" reads
  `process::log_tail` and shows it (`conflict: startFailed`, `conflictDetail`).

## Secrets

- Secrets live only in `state.json` (`ToolState.secrets`) and the rendered config files, both
  written 0600 via `paths::write_atomic(_, _, secret = true)`. `consumers.json` (grant keys) and
  `roadie-api.json` (bearer) likewise.
- Public shapes never carry them: `state::public_config` emits `has_<key>` booleans;
  `api::public_status` strips `pid`; dry runs mask secrets as `<secret>`. A test asserting a
  secret never appears in a response is the norm (`connection_needs_consent_and_config_never_echoes_secrets`).
- The MCP server redacts `apiKey|token|key|password|secret` fields unless a tool is explicitly
  asked with `includeSecret`.

## Naming and wire shapes

- Rust structs that cross to the window or the API are `#[serde(rename_all = "camelCase")]`;
  internally-tagged enums also need `rename_all_fields`. TypeScript mirrors live in
  `src/types.ts` — change both.
- Recipe JSON is camelCase (`recipeVersion`, `stripTopDir`, `startupGraceSec`). Placeholders
  are `{root.key}`; filters `|int`, `|json`, `|shell`.
- Events: `tool-status-changed {name}` (a nudge, re-read), `tool-install-progress`,
  `request-changed` (full request), `recipe-changed`, `intent`, `intent-error`. Emit through
  `events::emit` so API handlers and background threads can do it without an `AppHandle`.

## Tests

- Pure logic gets a unit test next to it; the recipe validator's tests assert **pointers**,
  because pointers are the product for assistant authors.
- API tests use `tower::ServiceExt::oneshot` against `build_router` with a fixed token and a
  temp data root — no network, no window.
- Anything that talks to the real network or spawns a real tool is `#[ignore]` (`tools/probe.rs`,
  `src/e2e.rs`, `tests/e2e_process.rs`); `npm run test:e2e` runs them all. `src/e2e.rs` is the
  client's view with approvals: the service in-process on a temp data dir, Viboplr over HTTP, the
  user's clicks through the owner channel. `tests/e2e_process.rs` is the real binary from "Roadie
  closed": CLI starts the service on demand, stop, idle exit in plain-app mode, a request opens a
  window. Each uses its own `--data-dir` with background mode off, so the real installation and
  login item are never touched. Extend them when a client-visible flow changes.
- MCP: `node --test mcp/*.test.mjs` with a fake `fetch`; the server must never need Roadie
  running to pass its tests.
- Frontend: vitest for pure helpers (`stateLabel`, formatters); `tsc --noEmit` must be clean.

## Dependencies

No new crate or npm package for a convenience. Ask what the dependency-free version looks like
first (a hand-written plist beats a plist crate; `reg.exe` beats a registry crate; the JSON
string trick beats a YAML crate). The owner has rejected such additions before.

## Git

Do not commit or create branches unless asked. The owner reviews and commits.
