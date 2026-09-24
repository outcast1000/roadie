---
paths:
  - "src-tauri/src/api/**"
  - "src-tauri/src/scheme.rs"
  - "src-tauri/src/consent.rs"
  - "src-tauri/src/requests.rs"
---

# Local API, deep links, consent, requests

## Transport and tiers (`api/mod.rs`)

- `127.0.0.1`, fixed port 47630, fallback 47631–47639. Discovery file `roadie-api.json`
  (0600): `{app, version, apiVersion, port, pid, token}`; the token is minted once and reused
  across restarts so a configured client keeps working.
- `host_and_method_guard`: `OPTIONS` → 405, `Host` must be loopback. No `Access-Control-*`
  header, ever — a web page must not be able to use a visitor's browser as a proxy into it.
- **Tier 1 (no token)**: `/v1/health`, `/v1/tools`, `/v1/tools/{name}`,
  `/v1/tools/{name}/connection?consumer=<id>` (403 `consent-required` until approved; a known
  consumer's 403 queues a Connect request, an unknown id does **not** auto-register — a name is
  not a credential).
- **Tier 2 (bearer, SHA-256-compared)**: start/stop/restart/update/check-updates/autostart/
  config/logs, the request queue, recipes (`schema`, `validate`, `PUT` draft, `dryrun`,
  `DELETE`, `?full=true` for stored shapes), consumers, `GET /v1/events?since&wait`
  (long-poll of `events.rs`'s numbered log), `POST /v1/shutdown` (the window's handoff when it
  replaces a stale build). `DELETE /v1/tools/{name}` sits on the public router path but checks
  the token itself.
- **Tier 3 (owner, `X-Roadie-Owner`)**: `/v1/owner/requests/{id}/decide`,
  `/v1/owner/recipes/{name}/trust`, `/v1/owner/tools/{name}/install` (the card's click),
  `DELETE /v1/owner/tools/{name}`, `/v1/owner/intent` (deep links), `/v1/owner/settings`.
  Tokens exist only in `owner.rs`'s registry, minted for a peer that connected to the owner
  socket **and** runs the Roadie binary (kernel-reported pid → `process::pid_exe`). A bearer
  token never opens these; a test asserts it (`owner_routes_need_a_channel_token…`).
- Handlers call `tools::*` / `store::*` under `spawn_blocking` (they do network and process I/O,
  and `reqwest::blocking` cannot be built inside the runtime) and end with `events::tool_changed`.
- Callers identify themselves with `X-Roadie-Client`; it becomes `requestedBy` on prompts.
- Every request except the `/v1/events` long-poll counts as activity (`service::touch`) for the
  plain-app idle exit. A handler that needs the user calls `scheme::focus_if_possible`, which
  also opens a window when none is connected.

## Requests (`requests.rs`)

Install and uninstall from any client, and Connect from deep links or the consent 403, are
`Request`s: in memory, deduped while pending, `pending → approved → done|failed` or `declined`,
with install progress mirrored in. `actions::decide`, reached only through the owner route, is
the only place they are answered — that is the user's click. Never resolve one from a public or
bearer handler.

## Consent (`consent.rs`)

`consumers.json` (0600): `id → {displayName, returnPrefix, tools: {tool → {key}}}`. `viboplr`
is built in. `approve(consumer, recipe)` mints a key sized by the recipe's `connection`
policy and is idempotent; the key reaches the tool by re-rendering its files
(`tools::refresh_consumers`, restart when idle). `revoke` drops the grant and re-renders.
Keys are served only by `/connection` (approved consumer, or bearer for the internal key).

## Deep links (`scheme.rs`)

`roadie://install|connect/<tool>?consumer=<id>&return=<url>` and `roadie://open/<tool>`.
Rules: the tool must be a known recipe (unknown → `intent` with `known:false`, window explains);
the consumer must be known; `return` must start with the consumer's registered prefix and never
be http(s). A link never installs anything — it focuses the row and, with a consumer, queues a
Connect request; approval opens `return?status=connected&tool=<t>` **without the key** (the
consumer fetches `/connection`). Three entry points (single-instance argv, `on_open_url`, macOS
`RunEvent::Opened`) all land in `scheme::handle` in the window, which forwards the URL to the
service's `/v1/owner/intent`; `actions::intent` parses, queues and emits `intent`.

## Tests

`api/mod.rs` tests build the router with a fixed token and a temp data root and assert: public
reads, Host/OPTIONS, 401s, consent 403 vs 200, 202 + pending for install, no secret echo, draft
not installable (409), built-in name collision (409). Extend them with any new route.
