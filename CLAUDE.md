# CLAUDE.md — Roadie

Guidance for AI agents working in this repository. `AGENTS.md` points here. Detailed rules
live in `.claude/rules/` and load by path (see the end of this file).

## What this is

Roadie is a standalone Tauri 2 desktop app (Rust backend, React/TypeScript frontend) that
installs, configures, runs, updates and shares the tools other apps depend on — slskd first,
then yt-dlp, ffmpeg, rqbit, cloudflared. It is **two processes from one binary**: `roadie
--serve` is the background **service** (API, engine, request queue, updates; `service.rs`),
and plain `roadie` is the **window**, a thin Tauri client that starts the service if needed and
relays the user's clicks over the local API (`commands.rs` → `client.rs`). It exists because its owner **rejected** having the
music player Viboplr (`outcast1000/viboplr`) install or run third-party daemons itself: Roadie
takes that responsibility, and Viboplr's plugins only *advertise* it (open a `roadie://` link,
read the localhost API). Roadie must never look or read like a Viboplr component — no Viboplr
branding, no Viboplr-specific code paths; Viboplr is one registered consumer among any.

## The four rules everything hangs on

1. **Every tool is a recipe, never Rust.** `recipes/*.json` describe a tool declaratively and
   `src-tauri/src/tools/` interprets them. A tool needing something new gets a new *generic*
   recipe field (schema in `recipes/SCHEMA.md`, validator in `src-tauri/src/recipe/mod.rs`, a
   built-in that uses it, a test). Never add `if recipe.name == "slskd"`. Recipes are also what
   AI assistants author through the API and MCP, so validation errors must name a JSON pointer
   and a fix.
2. **Nothing installs without the user's click.** Install and uninstall over the API are
   *requests* (`requests.rs`) the user approves in the window. A recipe that arrives through the
   API is a **draft** (`recipe/store.rs`) until the user reads the review screen and clicks
   Trust. An app may instead *bring* its recipe with an install or upgrade (`{"recipe": …}`, or
   a `.json` path in the CLI): identical to the trusted one it changes nothing; new or changed,
   it rides in the request (`recipeChange`), the prompt shows it for review, and approving trusts
   it (`store::put_trusted`) and then acts. It is stored nowhere before that click. Automatic *updates* of installed tools are allowed (on by default, toggleable) but never
   restart a busy daemon (`busy` check, staged versions).
3. **Daemons are independent and loopback-only.** They outlive Roadie (detached, `setsid` /
   hidden console), bind `127.0.0.1`, stop through a ladder (recipe API route → SIGTERM or
   Ctrl-Break → kill), and start at login only because the **service** starts them in its
   startup reconcile (`ToolState.autostart`); the one login item is the service's own
   (`roadie --serve --data-dir <dir>`), never a daemon's path.
4. **Only the user's own screen can act as the user.** Approving a request, trusting a draft,
   the card's Install/Remove and settings are **owner routes** (`/v1/owner/*`). The bearer token
   does not open them: an owner token comes only from the **owner channel** (`owner.rs`), a
   credentialed local socket where the service checks the peer pid runs the Roadie binary. This
   is what keeps rule 2 true against other programs running as the same user. The build without
   the window (`--no-default-features`) has the service show a native dialog instead
   (`prompt.rs`) and decide from its answer. A terminal gets an owner token only on a machine
   with no screen, because a program can drive the CLI through a pseudo-terminal it controls.
   There is no `--yes`, ever.

## Build, run, test

```bash
npm install --legacy-peer-deps          # npm 10.9's arborist trips on a peer set otherwise
npm run tauri dev                       # window; it spawns `roadie --serve` (API on 127.0.0.1:47630; Vite on 1430)
./src-tauri/target/debug/roadie --serve # the service alone, headless (logs to <data>/logs/roadie-service.log)
./src-tauri/target/debug/roadie tool status slskd        # CLI client (starts the service on demand); `roadie help`
./src-tauri/target/debug/roadie tool install slskd --wait # asks, opens the window for approval, exits 0/1/2
./src-tauri/target/debug/roadie tool install ./my-slskd.json --wait   # the app's own recipe: reviewed and trusted in the same prompt
cd src-tauri && cargo test              # engine, validator, emitters, API router (tower oneshot)
npm run test:mcp                        # node --test mcp/*.test.mjs
npx vitest run && npx tsc --noEmit      # frontend
cd src-tauri && cargo test --lib tools::probe -- --ignored --nocapture   # REAL install/start/stop of slskd (~60 MB download)
npm run test:e2e                        # REAL end-to-end: src/e2e.rs (in-process service, approvals via owner channel) + tests/e2e_process.rs (real binary lifecycle)
npm run tauri build -- --debug --bundles app   # a .app; the ONLY way to register the roadie:// scheme on macOS
cd src-tauri && cargo build --release --no-default-features --target-dir target/cli   # CLI + service only, no Tauri; requests answered in a native dialog
cd src-tauri && cargo test --no-default-features --target-dir target/cli             # the same tests without the window
```

- `roadie://` deep links do not work under `tauri dev` on macOS — LaunchServices learns the
  scheme from a bundle's Info.plist. Build the debug bundle and `open` it once.
- A local `tauri build` fails at the last step ("no private key") until `tauri signer generate`
  produces the updater key and `pubkey` in `tauri.conf.json` is filled; the `.app` is still
  produced. CI sets `TAURI_SIGNING_PRIVATE_KEY`.
- Data lives in Tauri's `app_data_dir` for `com.outcast1000.roadie`:
  `tools/<name>/{versions,data,logs}/`, `bin/` (cli shims), `recipes/` (user + `.draft.json`),
  `consumers.json`, `roadie-api.json` (0600, carries the API bearer token), `settings.json`,
  `owner.sock` (the owner channel), `logs/roadie-service.log`.
- The window replaces a service whose `buildId` (exe mtime+size) differs from its own, so a
  rebuilt dev binary never talks to a stale service. Settings → "Run in the background" off
  makes the service exit a few seconds after the window disconnects, or after 3 idle minutes
  when no window ever connected (plain-app mode). A request that needs the user while no window
  is connected makes the service open one (`service::open_window_if_needed`).
- The service's login item is `com.outcast1000.roadie.service` for the default data dir and
  `…service-<hash>` for any other, so a `--data-dir` sandbox never touches the real item. The
  window accepts `--data-dir` too (the service passes it when opening a window for a sandbox).
  `ROADIE_IDLE_EXIT_SECS` shortens the plain-app idle exit (tests use 4).
- Tauri's single-instance plugin means one window per user: a second launch is forwarded to
  the open window, so a window for another data dir cannot open while one is up.
- Three clients speak to the service: the window (owner channel), the MCP server, and the CLI
  (`roadie tool|request|service …`, `cli.rs`). The CLI asks; it approves only through
  `roadie request <id> answer` on a real TTY of a machine with no screen (SSH, headless), where
  the owner channel admits it as a `terminal`. Elsewhere `answer` re-shows the request on screen.
- Where a request is shown is `prompt::surface()` (`approvalSurface` in `/v1/health`): `window`
  in the desktop build, `dialog` without it (osascript / MessageBoxW / zenity or kdialog, text
  passed as arguments, never as script), `terminal` when the OS says there is no screen (macOS
  session graphic access, Windows visible window station, Linux `DISPLAY`/`WAYLAND_DISPLAY`).
- Driving the running app from a shell: read the token from `roadie-api.json` and curl
  `127.0.0.1:47630` — or speak MCP to `mcp/roadie-mcp.mjs` over stdio, as a client would.

## Layout

| Path | What |
|---|---|
| `recipes/` | built-in recipes (compiled in with `include_str!` in `recipe/mod.rs` → `BUILTIN`) + `SCHEMA.md` |
| `src-tauri/src/recipe/` | recipe types + validator, `template.rs` (placeholders, `$each`, `$if`), `emit.rs` (yaml/json/env/ini), `jsonq.rs`, `httpsteps.rs`, `store.rs` (builtin/user/draft) |
| `src-tauri/src/tools/` | the interpreter: `mod.rs` (status, liveness, install/start/stop/configure, reconcile, auto-update, dry run), `install.rs`, `process.rs`, `autostart.rs`, `state.rs`, `probe.rs` |
| `src-tauri/src/api/` | axum local API: public tier, bearer tier (incl. `/v1/events` long-poll), owner tier, requests, recipes, consumers |
| `src-tauri/src/{service,owner,actions,client}.rs` | the service entry point; the owner channel; the user's actions (decide/trust/install-now); the window's HTTP client + event pump |
| `src-tauri/src/{scheme,consent,requests,events,cli,commands,mcp_setup,paths}.rs` | deep links, consumer grants, approval queue, event log, CLI modes, Tauri commands (relays; `window` feature only) |
| `src-tauri/src/prompt.rs` | approval surface: prompt text for a request, native dialogs, "is there a screen", the service's dialog queue |
| `src/` | React window: `hooks/`, `components/` (ToolRow, ConfigForm, RecipeReview, RequestPrompt, SettingsPane) |
| `mcp/` | dependency-free stdio MCP server, bundled into `Resources/mcp/` |

## Do not

- Add a tool-specific branch in Rust or TypeScript. Extend the recipe format instead.
- Install, uninstall or trust anything from a public or bearer API handler. Queue a request /
  save a draft. Only owner routes call `actions::{decide,trust,install_now,uninstall_now}`.
- Put engine logic in `commands.rs`; it is a relay. Engine behaviour belongs in `tools/` and is
  reached through the API so the window and every other client see the same thing.
- Return a secret (tool API key, bearer token) from a public route or a Tauri status payload.
  `state::public_config` and `ToolPublic` are the shapes; `has_<key>` booleans stand in.
- Call `api.github.com`. Release lookup is `HEAD github.com/<repo>/releases/latest` and reading
  the redirect (`install.rs`), because the API's per-IP budget is exhausted on shared egress.
- Add crates for convenience. plist, `reg.exe`, `.desktop`, YAML emission and Windows FFI are
  all hand-rolled on purpose; `serde_json` has `preserve_order` so emitted files keep the
  recipe author's key order.
- Add CORS to the API, or accept a `Host` that is not loopback.

## Rules (path-scoped, in `.claude/rules/`)

- `conventions.md` — always loaded: error handling, feedback, naming, tests.
- `engine.md` → `src-tauri/**`: interpreter invariants, state, ports, stop ladder, updates.
- `recipes.md` → `recipes/**`, `src-tauri/src/recipe/**`: the format and how to extend it.
- `api.md` → `src-tauri/src/api/**`, `src-tauri/src/{scheme,consent,requests}.rs`: tiers, requests, consent, deep links.
- `frontend.md` → `src/**`: the window.
- `mcp.md` → `mcp/**`: the MCP server's contract.
