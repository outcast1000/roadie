---
paths:
  - "src/**"
---

# Frontend (src/)

A small React 19 window; no router, no state library. Two tabs (Tools, Settings) and a review
overlay. `src/types.ts` mirrors the Rust wire shapes — change it with them.

## Hooks

- `useTools` — `tool_list` on mount, re-read on `tool-status-changed` (coalesced 150 ms) and
  a 15 s poll (daemons change state on their own). Every mutation goes through `run(name,
  label, op)`: sets `busy[name]`, clears/sets `errors[name]`, refreshes after. Install progress
  from `tool-install-progress` lands in `installing[name]`.
- `useRequests` — pending prompts; `request-changed` moves them to `recent`; `decide(id,
  approve, answers?)` is the user's click; `answers` are install decisions typed in the prompt.
- `useRecipes` — list with origin; `trust` / `remove` / `dryRun`.

## Components

- `ToolRow` — one card per recipe. `stateLabel()` decides the one-line state (draft → not
  supported → not installed → cli installed → conflicts → running/healthy → starting → stopped);
  buttons appear per state; `ConfigForm` renders `recipe.config`; the log panel polls
  `tool_logs` every 3 s while open. Removal is a two-step inline confirm with "keep settings"
  and "remove everything".
- `ConfigForm` — password fields show only whether a value exists; blank keeps, Clear sends
  `""`. Path fields use the Tauri dialog; `tccSensitive` paths under macOS-protected folders warn.
- `RecipeReview` — the trust screen. It must make the download URLs, the run command and env,
  and every rendered file (from a dry run) impossible to miss; Trust and Delete live here.
- `RequestPrompt` — one pending request; Approve is disabled while the tool's recipe is an
  untrusted draft. An install request shows `InstallPlan` (a dry run: download, paths, files,
  ports) and a `ConfigForm` over the recipe's `askOnInstall`/`required` fields, pre-filled with
  what the client sent (`request.config`, `secretKeys`). A request that brought a recipe
  (`request.recipe`, `recipeChange`) plans and asks from *that* recipe, links to `RecipeReview`
  on it (read-only, no Trust/Delete: the prompt approves), and keeps its approve button
  ("Trust recipe and install/update") disabled until that review was opened.
- `InstallPlan` — "what will change on this computer", built from `recipe_dry_run`. Shown before
  every install, from the card (`ToolRow` Install → inline prompt) and from a request.
- `SettingsPane` — auto-update toggle, app self-update (prompted, never silent), connected apps
  with per-tool revoke, API info, the MCP card (Copy config / Copy command with absolute paths).

## Rules

- No hardcoded colours: everything goes through the `:root` variables in `App.css`, which has a
  dark scheme. New UI must read in both.
- Never render a secret. The wire never carries one, so a field named like one appearing in
  the UI means a backend shape leaked — fix the backend.
- Errors stay visible until dismissed (`callout error` with Dismiss), never a toast that fades.
- `invoke` names are the Rust command names in `commands.rs`; keep them in one place per hook.
  Every command is a relay to the service's API, so a wire shape is the API's shape.
- The event pump (`client.rs`) mirrors the service's log as the same event names; it also emits
  `service-changed {connected}` which `App` turns into a banner with Retry.
