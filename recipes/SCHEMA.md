# Roadie recipe format (recipeVersion 1)

A recipe is one JSON document describing how Roadie installs, configures, runs and updates a
tool. Roadie has no code that knows a tool by name; everything a tool needs is a field here.
Recipes never contain secrets or user values — those live in the tool's `state.json`.

Validation reports every error with a JSON pointer (`/files/0/content/web/port`) and a message,
so fix the named field and re-validate. `GET /v1/recipes/schema` returns this text plus a full
built-in example. Copy the closest built-in and edit it.

## Top level

| field | type | notes |
|---|---|---|
| `recipeVersion` | `1` | required |
| `name` | string | `^[a-z0-9][a-z0-9-]{0,31}$`, unique across all recipes |
| `author` | string | required; who maintains the recipe (a person, project or organisation). Shown on the card and the review screen |
| `revision` | integer ≥ 1 | required; the recipe's own edition. Start at 1 and bump it on every change. Not the tool's version, not `recipeVersion` |
| `platforms` | `[ "<platform>" ]` | required; the platforms this recipe targets. Every listed platform must have a download in `source` or an override, and every download must be listed |
| `displayName`, `summary`, `homepage`, `license`, `notes` | string | `notes` is free text for consumers (e.g. "never pass `-U`") |
| `kind` | `"daemon"` \| `"cli"` | daemons run and are health-checked; cli tools are installed and exposed as `bin/<name>` |
| `source` | object | where releases come from (below) |
| `archive` | `"zip"` \| `"tgz"` \| `"bare"` | `bare` = the download *is* the binary |
| `layout` | `{ stripTopDir, binaries }` | `binaries[0]` is the main binary; relative paths inside the archive; `.exe` is added on Windows |
| `minBinaryBytes` | number | size floor for the main binary (catches HTML error pages) |
| `version` | `{ args, regex, timeoutSec }` | run after extraction; the regex's first capture must agree with the resolved version |
| `secrets` | `[ { key, generate: "hex<N>" } ]` | generated once, stored in state, available as `{secrets.key}` |
| `ports` | `{ name: { default, pick } }` | daemon only; `pick: true` scans `default+1..+10` when the default is taken by something else |
| `config` | `[ ConfigField ]` | user-editable values (below) |
| `connection` | `{ policy, minLen, maxLen, url }` | `policy`: `none` \| `open` \| `perConsumerKey` |
| `createDirs` | `[ string ]` | created before start (tools that refuse a missing directory) |
| `files` | `[ FileDef ]` | config files written before every start |
| `run` | `{ args, env, cwd, startupGraceSec }` | daemon only, required |
| `health` | `{ request, unauthorizedStatus, extract }` | daemon only, required |
| `busy` | `{ requests, busyIf: { path, regex } }` | when any reply has a value at `path` matching `regex`, the daemon is busy and must not be restarted |
| `stop` | `{ graceSec, api: [ HttpRequest ] }` | graceful stop through the tool's API; then SIGTERM/Ctrl-Break; then kill |
| `startAfterInstall` | `{ default, askOnInstall }` | daemon only; Roadie starts the daemon right after the first install. Decision key `startNow` |
| `autostart` | `{ default, askOnInstall }` | daemon only; register a login item (Roadie's launcher mode, never the daemon's own path). Decision key `autostart` |
| `startFailures` | `[ { regex, code, message } ]` | matched against the log when the process dies during startup |
| `logExtract` | `{ key: regex }` | `details.<key>` read off the log (first capture) |

## Source

```jsonc
{ "kind": "githubRelease", "repo": "owner/repo",
  "tagStyle": "plain" | "vPrefixed" | "floating:<tag>",
  "assets": { "<platform>": "name-{version}-something.zip", ... },
  "checksums": { "kind": "none" } | { "kind": "sumsFile", "asset": "SHA2-256SUMS" } | { "kind": "sidecar", "suffix": ".sha256" } }

{ "kind": "httpRedirect",
  "latestUrl": { "<platform>": "https://.../latest/..." },   // redirects to the versioned file
  "versionRegex": "ffmpeg-(\\d+\\.\\d+(?:\\.\\d+)?)",         // applied to the final URL
  "checksums": { ... } }

{ "kind": "htmlIndex",
  "page": "https://ffmpeg.martin-riedl.de/",                  // a download page with versioned links
  "links": { "<platform>": "^/download/macos/arm64/\\d+_(\\d+\\.\\d+(?:\\.\\d+)?)/ffmpeg\\.zip$" },
  "checksums": { "kind": "sidecar", "suffix": ".sha256" } }   // regex vs every href; capture 1 = version; highest wins
```

When one tool is packaged differently per OS, `overrides` replaces `source` / `archive` /
`layout` for a platform (each key optional):

```jsonc
"archive": "zip", "layout": { "binaries": ["ffmpeg"] },
"overrides": { "windows-x64": {
  "source": { "kind": "githubRelease", "repo": "BtbN/FFmpeg-Builds", "tagStyle": "floating:latest", ... },
  "layout": { "stripTopDir": true, "binaries": ["bin/ffmpeg", "bin/ffprobe"] } } }
```

Platforms: `darwin-arm64`, `darwin-x64`, `windows-x64`, `windows-arm64`, `linux-x64`, `linux-arm64`.
`platforms` lists the ones the recipe targets; a platform not listed (or without a download) means
"not available on this computer". The validator points at `/platforms/<i>` for a listed platform
with no download and at `/platforms` for a download whose platform is not listed. Latest-release lookup uses
`HEAD github.com/<repo>/releases/latest` and reads the redirect, never `api.github.com`.

## ConfigField

```jsonc
{ "key": "downloadsDir", "label": "Downloads folder", "help": "...",
  "kind": "text" | "password" | "path" | "bool" | "port",
  "required": false, "secret": false, "default": "{home}/Music/Tool",
  "tccSensitive": true,   // macOS: warn when under ~/Downloads|Documents|Desktop
  "createDir": true,      // path fields: create the directory before start
  "askOnInstall": true }  // a decision to make before the first install (see below)
```

`askOnInstall` marks the values a person has to decide before the tool is first installed — an
account name, a folder. `required` fields are always asked. When the user clicks Install in
Roadie, the window asks for exactly these fields first. When an API or MCP client asks to
install, it may pass them in the request body (`POST /v1/tools/<name>/install` with
`{ "config": { "<key>": <value>, … } }`); the approval prompt shows what the client decided,
asks the user for anything still missing, and the user can change any of it before approving.
Secret (password) fields may be passed the same way; they are stored 0600 and never echoed
back by `GET /v1/requests/<id>`.

Two decisions belong to the engine rather than to a config field, and use reserved keys in the
same `config` object: `startNow` (start the daemon as soon as it is installed) and `autostart`
(start it at login). A recipe offers them with `startAfterInstall` / `autostart` at the top
level, each `{ "default": true|false, "askOnInstall": true|false }`; the prompt pre-ticks the
default and the user can flip it. Passing either key for a recipe that does not offer it is a
422. Roadie's own Start/Stop on the tool's card work regardless.

`password` fields must be `secret: true` and go to `{secrets.key}`; everything else is
`{config.key}`. Patching a password with `""` clears it; omitting the key keeps it.

## FileDef and placeholders

```jsonc
{ "path": "{data}/tool.yml", "format": "yaml" | "json" | "env" | "ini" | "raw", "secret": true,
  "content": { ... JSON tree with placeholders ... } }
```

`path` must start with `{data}`. The content is a JSON tree; Roadie expands placeholders and
serializes it in the named format, so quoting is by construction (a password full of `#`, `:`
and quotes cannot break a YAML file).

Placeholders, in any string: `{home}` `{data}` `{bin}` `{version}` `{platform.os}`
`{platform.arch}` `{ports.X}` `{config.X}` `{secrets.X}` `{connection.url}`. Filters:
`{ports.web|int}` emits a number, `{x|json}` a JSON-quoted string, `{x|shell}` a shell-quoted
string. A string that is exactly one placeholder keeps the value's type (a bool stays a bool).
`{{` and `}}` are literal braces.

Two directives inside `content`:

```jsonc
"api_keys": { "roadie": { "key": "{secrets.internalKey}" },      // static entries are kept
              "$each": "consumers", "key": "{item.id}",           // one entry per approved consumer
              "value": { "key": "{item.key}", "role": "readwrite" } }

"directories": { "$if": "config.shareDownloads", "then": ["{config.downloadsDir}"], "else": [] }
```

A `$if` without `else` that is false drops the key.

## HttpRequest

```jsonc
{ "method": "GET", "url": "{connection.url}/api/v0/application",
  "headers": { "X-API-Key": "{secrets.internalKey}" }, "json": { ... },
  "capture": { "token": "$.token" }, "timeoutSec": 5 }
```

`capture` reads values off the JSON reply with a tiny query language — `$.a.b`, `$[0]`, `$[*]`,
`$..state` (recursive) — and makes them available as `{token}` to later steps of the same chain.
`health.extract` uses the same queries: `"version": "$.version.current"`,
`"details.soulseekConnected": "$.server.isLoggedIn"`.

## Policies Roadie applies to every daemon

- Bind to `127.0.0.1`. A recipe that binds elsewhere will be visible to the user in the review
  screen (every `run.args` and every rendered file is shown) and should not be trusted.
- Nothing installs without a user click. `PUT /v1/recipes/<name>` saves a **draft**; the user
  must Trust it in Roadie before it can be installed.
- A running daemon is never restarted while `busy` says so; updates and config changes wait.

## Authoring loop for assistants

1. `GET /v1/recipes/schema` — this text and the slskd example.
2. `GET /v1/recipes/<closest>` — copy and edit.
3. `POST /v1/recipes/validate` until `ok: true`.
4. `PUT /v1/recipes/<name>` — saved as a draft; tell the user to review and Trust it in Roadie.
5. `POST /v1/recipes/<name>/dryrun` — resolves the release for this platform, checks the asset
   URL answers, and renders the files with placeholder values. Nothing is downloaded or written.
6. `POST /v1/tools/<name>/install` — a request the user approves; poll `GET /v1/requests/<id>`.
