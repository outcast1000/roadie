---
paths:
  - "recipes/**"
  - "src-tauri/src/recipe/**"
---

# Recipes

`recipes/SCHEMA.md` is the normative description of the format and is served verbatim by
`GET /v1/recipes/schema`, so keep it exact and example-driven: its readers are AI assistants
writing recipes from it. When a field changes, update SCHEMA.md in the same change.

## Where recipes come from

- **Built-in**: `recipes/<name>.json`, listed in `recipe::BUILTIN` (`include_str!`). A test
  (`every_builtin_validates_and_names_match`) parses all of them; an invalid built-in is a
  compile-time-adjacent failure, never a runtime surprise.
- **User**: `<data>/recipes/<name>.json` — a draft the user trusted, or a recipe an app brought
  with an install/upgrade request that the user approved (`store::put_trusted`). It may carry a
  built-in's name and then replaces that built-in, until Roadie ships the built-in at a higher
  `revision` (`store::merge`); deleting it gives the name back to the built-in.
- **Draft**: `<data>/recipes/<name>.draft.json`, wrapped `{"submittedBy", "recipe"}`; not
  installable. `store::trust` rewrites it as a user recipe. Names are unique across all three; a
  draft that shadows anything is skipped with a warning.
- **Brought**: a recipe in a request (`RequestKind::Install.recipe`, `ReplaceRecipe`), compared by
  `store::compare` (parsed values, so key order does not count). Never on disk until approved.

## Writing one

- Copy the closest built-in. `slskd.json` is the daemon reference (ports, secrets, `$each` over
  consumers, JWT stop chain, `startFailures`); `yt-dlp.json` the simplest cli; `ffmpeg.json`
  shows `htmlIndex` and per-platform `overrides`.
- Prefer upstream checksums (`sumsFile`, `sidecar`); with `none`, say in `notes` that the copy is
  verified by running it.
- Daemons bind `127.0.0.1`. A `files[].content` that binds elsewhere is a recipe the review
  screen should make the user distrust; don't add engine code to "fix" it.
- Config fields that are secrets are `kind: password, secret: true` and flow to `{secrets.X}`;
  everything else is `{config.X}`. Directories a daemon refuses to start without go in
  `createDirs` or a path field's `createDir`.
- `files[].path` must start with `{data}` — a recipe writes only inside its own data dir.
- Prefer structured `content` over `format: raw`; the emitters quote by construction.

## The engine side (`src-tauri/src/recipe/`)

- `mod.rs` — types (serde, camelCase, `deny_unknown_fields` deliberately **off** so an older
  Roadie reads a newer recipe's additive fields), `validate()` returning
  `Vec<ValidationError {pointer, message}>`, `Platform`, `Recipe::{source_for, archive_for,
  layout_for, binaries, supported_on}`.
- `template.rs` — `{placeholder}` expansion with typed single-placeholder values, `$each`
  (static keys kept), `$if/then/else`; `placeholders()` and `split_filter()` back the validator.
- `emit.rs` — YAML (strings as JSON strings → safe scalars), JSON, env, ini, raw.
- `jsonq.rs` — `$`, `.key`, `[n]`, `[*]`, `..key`.
- `httpsteps.rs` — `send()` and `run_chain()` with captures; `SendError::Unreachable` is the
  signal liveness keys on.

Validation errors are the product: when you add a check, the message must tell an assistant
what to change (`"`{ports.wev}` names nothing this recipe declares"`), and a test asserts the
pointer.
