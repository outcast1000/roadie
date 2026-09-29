---
paths:
  - "src-tauri/**"
---

# Engine (src-tauri/src/tools/, paths.rs, cli/)

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
- **tools/autostart.rs** — login items, hand-rolled (LaunchAgent plist, HKCU `Run` via
  `reg.exe`). **A daemon's own item** (`tool-<name>`, macOS only, `NATIVE_TOOL_ITEMS`): runs `launch_plan()` — the binary of the current version, expanded
  args, env, cwd, output appended to `stdout.log`; `KeepAlive false` so Stop sticks. Written
  only when it changed and **never bootstrapped** (a `RunAtLoad` load would start a second copy
  now), removed as a file only (a `bootout` would stop the running daemon). `tool_item_pid`
  reads `launchctl list <label>` so liveness can adopt a daemon launchd started and write its
  pid file. Also: the **service's** item (`roadie --serve`, `com.outcast1000.roadie.service`),
  the CLI's `maintain` item (Windows only; on macOS every CLI command removes it after syncing
  the daemons' items), and the `--start-tool` items of older builds, removed when seen.
  Item names get a `-<hash>` suffix for a non-default data dir.
- **Login-item sync** (`tools::sync_login_item`, under the tool's lock): after install,
  set_autostart, start (ports), apply_pending (version), configure, and in reconcile; uninstall
  removes the item. At login (`reconcile_with(.., at_login = true)`) a daemon with its own item
  is launchd's to start; Roadie starts it only when no item existed yet (the first login after
  an upgrade) or on Windows.
- **tools/state.rs** — `state.json` (`ToolState`): ports, secrets, config, flags.
  `load_or_init` mints `secrets[]`, fills port defaults and expands config defaults;
  `apply_patch` validates against `recipe.config` (password `""` clears, absent keeps).
- **cli/** — `mod.rs` parses modes and holds what both releases share (`Target`, `--as`,
  `recipe validate`, the TTY prompt). Desktop (`service` feature): `--serve` runs
  `service::run`, the legacy `--start-tool` ensures the service is up, and `remote.rs` is the
  API client (`request list`, `request <id> answer`). CLI release: `local.rs` runs every command
  in-process through `intake` and `actions`, asks in `prompt::dialog`, and keeps the `maintain`
  login item in step. `maintain` is `reconcile_with(.., at_login)`: without `--at-login` a
  daemon the user stopped stays stopped. The update pass runs when `<data>/last-update-pass`
  is a day old.
- **service.rs / owner.rs / actions.rs / client.rs** — the process split: see CLAUDE.md rules 3
  and 4. `service::build_id()` (exe mtime+size) is how the window detects a stale service.
- **paths.rs** — one data root (`OnceLock`), `ToolPaths {versions, data, logs}`, `bin_dir`,
  `write_atomic(path, bytes, secret)`.

## Invariants

- **macOS and Windows only.** Every OS-specific path has exactly a `target_os = "macos"` (or
  `unix`, which means the same thing — `lib.rs` has a `compile_error!` for anything else) and a
  `windows` branch. Never add a Linux or "other unix" fallback.
- **Per-tool lock** around every mutation (`lock(name)`); reads (`status`) are lock-free. It
  is an in-process mutex plus a file lock in `<data>/locks/<name>.lock` (`paths::lock_file`,
  `flock` / `LockFileEx`), because CLI-release runs are separate processes. Never nest it.
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

- **Options** (`intake::options`) are the one list of what an install takes, the same for the
  CLI, the API and MCP. Any port or secret may be given (`askOnInstall` only drives Roadie's
  prompt). A secret without `generate` is required: `intake::install` refuses without it where
  nothing can ask, and `require_secrets` refuses at approval.
- **Another copy** (`other_instance`): before install, the recipe's own health check on the
  ports the install would use (and, for a `singleton`, the defaults). An answer that rejects
  Roadie's key means another copy. It is a warning in options, the dry run, the install reply and
  the prompt, never a block, and Roadie never stops the other copy. It stays quiet while Roadie's
  own copy is the one running.
- **Install progress** is also a marker, `<data>/installing.json` (`tools::install` writes it
  and always removes it). `status.installing` reads it in any process and ignores a marker
  whose pid is dead.
- **Choices at install** (`install_options` → `apply_install_choices`, before the first
  install): `installDir` is written to `<tool>/install-dir`, which `paths::tool_paths` reads, so
  every path helper follows it. It must be new or empty, since uninstall removes it. `ports.<name>`
  go into `ToolState.chosen_ports`, which `choose_ports` never moves. `secrets.<key>` are kept
  as given (`fill` only regenerates an `askOnInstall` secret shorter than `minLen`).
- **`writeOnce` files** are rendered only when absent (`write_files`). Once one exists
  (`files_frozen`), ports stop moving and the chosen values it carries are refused a change. When
  every file is `writeOnce` (`settings_frozen`), `configure` with a patch errors and status says
  `configurable: false`.
- **Connection policies**: `perConsumerKey` re-renders the config on every grant change
  (`refresh_consumers`). `sharedKey` and `open` have nothing to re-render, and
  `intake::consumer_connection` hands out the secret `connection.key` names.

## Adding a capability

New engine behaviour is driven by a new recipe field: type in `recipe/mod.rs`, validation with a
pointer, a line in `recipes/SCHEMA.md`, use in the interpreter, a recipe that exercises it (a
fixture, then the catalog), a unit test. `archive: tgz` is declared but not implemented yet (returns an error) — the first
recipe that needs it adds `tar`+`flate2` and a traversal test.
