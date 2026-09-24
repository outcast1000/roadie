---
paths:
  - "src-tauri/**"
---

# Engine (src-tauri/src/tools/, paths.rs, cli.rs)

The interpreter for recipes. It knows no tool by name; every behaviour comes from the
`Recipe` it is handed. Ported from a rejected in-app sidecar, so several rules below were bugs
there first.

## Files

- **tools/mod.rs** — `status()` (the one read; `ToolStatus` is what the window and the API
  see), `liveness()` (pid file cross-checked with `process::is_ours` + the recipe's health probe;
  cleans stale pid files; adopts an unspawned instance that accepts our key), `restart_allowed()`
  (the recipe's `busy` requests; an error means busy), `install / start / stop / restart /
  set_autostart / configure / uninstall`, `apply_pending()` (staged version and/or pending
  restart, never on a busy daemon), `reconcile()` (startup pass), `auto_update()` (daily),
  `dry_run()` (resolve + render, no download, no write, secrets masked), `render_files()`.
- **tools/install.rs** — `resolve_latest()` per `Source` kind (GitHub redirect, HTTP redirect,
  HTML index), `download_and_stage()` (size floor, `<`-sniff, checksums when the source has
  them, extract, `stripTopDir`, chmod, run the version flag, stamp, rename), versions dir
  helpers (`current` pointer file, staged versions newest-first, prune), `probe_version()`,
  `output_with_timeout()` (kills the process group), `LatestCache` (24 h, failures cached).
- **tools/process.rs** — `spawn_detached` (unix `setsid`; Windows `CREATE_NO_WINDOW |
  CREATE_NEW_PROCESS_GROUP` — a *hidden* console so Ctrl-Break has something to attach to),
  `is_ours` (exe path under the versions dir), the stop ladder `stop()`, `log_tail`, hand-declared
  kernel32 FFI.
- **tools/autostart.rs** — the **service's** login item (`roadie --serve --data-dir <dir>`,
  label `com.outcast1000.roadie.service`): LaunchAgent plist (`KeepAlive false`,
  `AbandonProcessGroup true`), HKCU `Run` via `reg.exe`, XDG `.desktop`. A tool's "start at
  login" is `ToolState.autostart`, acted on by `reconcile()` at service start; per-tool items
  from older builds are removed when seen.
- **tools/state.rs** — `state.json` (`ToolState`): ports, secrets, config, flags.
  `load_or_init` mints `secrets[]`, fills port defaults and expands config defaults;
  `apply_patch` validates against `recipe.config` (password `""` clears, absent keeps).
- **cli.rs** — `--serve [--data-dir]` runs `service::run`; the legacy `--start-tool` just
  ensures the service is up. Both are intercepted before the Tauri builder.
- **service.rs / owner.rs / actions.rs / client.rs** — the process split: see CLAUDE.md rules 3
  and 4. `service::build_id()` (exe mtime+size) is how the window detects a stale service.
- **paths.rs** — one data root (`OnceLock`), `ToolPaths {versions, data, logs}`, `bin_dir`,
  `write_atomic(path, bytes, secret)`.

## Invariants

- **Per-tool lock** around every mutation (`lock(name)`); reads (`status`) are lock-free.
- **Never restart a busy daemon.** Updates and config changes go through `apply_pending`; when
  `restart_allowed` says busy/unreachable the change is *staged* (`ApplyOutcome::Deferred`) and
  lands at the next stop/start or when idle. `status` reports `updateStaged` /
  `updateDeferredReason` / `restartPending` so the window can say so.
- **Versions side by side**, `current` is a text file (a symlink would not survive Windows),
  prune keeps the current and the one before.
- **Verify by running.** Even with upstream checksums, the extracted main binary runs its
  version flag and the regex capture must prefix-match the resolved version (`floating:` tags
  skip the compare). No checksum upstream → size floor + HTML sniff + this run is the whole
  defence; say so in the review UI, not in code comments only.
- **Ports.** `choose_ports` keeps a port that is free or already answers as ours/foreign; scans
  `default+1..=+10` otherwise. A foreign instance on the port is *reported*
  (`foreignInstanceOnPort`), never fought.
- **Start failure classification** is the recipe's `startFailures` regexes over the last 40 log
  lines; the fallback is `startFailed` with the tail as detail. `last_errors` persists the
  verdict until the next successful start or an explicit stop.
- **Cli tools** never start; install refreshes `bin/<name>` (unix symlink, Windows copy with
  retry) and `status.binPath` is what consumers run.
- **Windows** paths are code-complete but only the owner can test them; say when a change
  touches `#[cfg(windows)]` code.

## Adding a capability

New engine behaviour is driven by a new recipe field: type in `recipe/mod.rs`, validation with a
pointer, a line in `recipes/SCHEMA.md`, use in the interpreter, a built-in that exercises it, a
unit test. `archive: tgz` is declared but not implemented yet (returns an error) — the first
recipe that needs it adds `tar`+`flate2` and a traversal test.
