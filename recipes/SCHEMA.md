# Roadie recipe format (recipeVersion 1)

A recipe is one JSON document describing how Roadie installs, configures, runs and updates a
tool. Roadie has no code that knows a tool by name; everything a tool needs is a field here.
Recipes never contain secrets or user values — those live in the tool's `state.json`.

Validation reports every error with a JSON pointer (`/files/0/content/web/port`) and a message,
so fix the named field and re-validate. `GET /v1/recipes/schema` returns this text plus a full
example from the recipe catalog. Copy the closest catalog recipe (`GET /v1/recipes/<name>`) and
edit it.

Roadie ships no recipes. It gets them from the recipe catalog,
[`outcast1000/roadie-recipes`](https://github.com/outcast1000/roadie-recipes): one file per tool,
added or changed by pull request, and reviewed by the user before Roadie installs it. To publish
a recipe, open a pull request there (its CONTRIBUTING.md has the rules).

## Top level

| field | type | notes |
|---|---|---|
| `recipeVersion` | `1` | required |
| `name` | string | `^[a-z0-9][a-z0-9-]{0,31}$`, unique across all recipes |
| `author` | string | required; who maintains the recipe (a person, project or organisation). Shown on the card and the review screen |
| `revision` | integer ≥ 1 | required; the recipe's own edition. Start at 1 and bump it on every change. Not the tool's version, not `recipeVersion` |
| `singleton` | bool | daemon only; the tool runs one copy per computer, so a second refuses to start whatever its port. Before an install Roadie then looks for another running copy on the default ports too, and the warning says Roadie's copy will not start until it is quit |
| `minRoadie` | string | optional; the oldest Roadie that runs this recipe correctly (`"0.6.0"`). Set it when the recipe uses a field a Roadie release added: an older Roadie ignores fields it does not know, so the catalog hides the recipe from it |
| `platforms` | `[ "<platform>" ]` | required; the platforms this recipe targets. Every listed platform must have a download in `source` or an override, and every download must be listed |
| `displayName`, `summary`, `homepage`, `license`, `notes` | string | `notes` is free text for consumers (e.g. "never pass `-U`") |
| `kind` | `"daemon"` \| `"cli"` | daemons run and are health-checked; cli tools are installed and exposed as `bin/<name>` |
| `source` | object | where releases come from (below) |
| `archive` | `"zip"` \| `"tgz"` \| `"bare"` | `bare` = the download *is* the binary |
| `layout` | `{ stripTopDir, binaries }` | `binaries[0]` is the main binary; relative paths inside the archive; `.exe` is added on Windows |
| `minBinaryBytes` | number | size floor for the main binary (catches HTML error pages) |
| `version` | `{ args, regex, timeoutSec }` | run after extraction; the regex's first capture must agree with the resolved version |
| `secrets` | `[ { key, generate: "hex<N>", askOnInstall, label, minLen } ]` | generated once, stored in state, available as `{secrets.key}`. With `askOnInstall` (and a `label`), the installing app or the user may choose the value instead (decision `secrets.<key>`, at least `minLen` characters, default 16), and the user can see it on the tool's card |
| `ports` | `{ name: { default, pick, askOnInstall, label } }` | daemon only; `pick: true` scans `default+1..+10` when the default is taken by something else. With `askOnInstall` (and a `label`) the app or user may choose it (decision `ports.<name>`); a chosen port is never moved |
| `config` | `[ ConfigField ]` | user-editable values (below) |
| `configuration` | `[ ConfigEntry ]` | settings of the tool's own config file an installing app or the user may set, by their real key there (below) |
| `connection` | `{ policy, key, minLen, maxLen, url, webLogin }` | `policy`: `none` \| `open` \| `perConsumerKey` (each approved app gets its own key, listed in the config with `$each: "consumers"`) \| `sharedKey` (every approved app gets the one secret `key` names, and talks to the tool directly with it). `webLogin` (optional): `{ username, password }` for the tool's own web page, placeholders allowed (below) |
| `createDirs` | `[ string ]` | created before start (tools that refuse a missing directory) |
| `files` | `[ FileDef ]` | config files written before every start, unless `writeOnce` |
| `run` | `{ args, env, cwd, startupGraceSec }` | daemon only, required |
| `health` | `{ request, unauthorizedStatus, extract }` | daemon only, required |
| `busy` | `{ requests, busyIf: { path, regex } }` | when any reply has a value at `path` matching `regex`, the daemon is busy and must not be restarted |
| `stop` | `{ graceSec, api: [ HttpRequest ] }` | graceful stop through the tool's API; then SIGTERM/Ctrl-Break; then kill |
| `startAfterInstall` | `{ default, askOnInstall }` | daemon only; Roadie starts the daemon right after the first install. Decision key `startNow` |
| `autostart` | `{ default, askOnInstall }` | daemon only; start at login: on macOS the daemon's own login item (its binary and run args), on Windows Roadie's item starts it. Decision key `autostart` |
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

Platforms: `darwin-arm64`, `darwin-x64`, `windows-x64`, `windows-arm64`. Roadie runs on macOS and
Windows only; there are no Linux platforms.
`platforms` lists the ones the recipe targets; a platform not listed (or without a download) means
"not available on this computer". The validator points at `/platforms/<i>` for a listed platform
with no download and at `/platforms` for a download whose platform is not listed. Latest-release lookup uses
`HEAD github.com/<repo>/releases/latest` and reads the redirect, never `api.github.com`.

## ConfigField

```jsonc
{ "key": "downloadsDir", "label": "Downloads folder", "help": "...",
  "kind": "text" | "password" | "path" | "paths" | "bool" | "port",
  "required": false, "secret": false, "default": "{home}/Music/Tool",
  "tccSensitive": true,   // macOS: warn when under ~/Downloads|Documents|Desktop
  "createDir": true,      // path fields: create the directory before start
  "askOnInstall": true }  // a decision to make before the first install (see below)
```

A `paths` field is a list of absolute folders (its `default` is a JSON array, usually `[]`).
Clients send it as a JSON array; on the CLI, `--set key=` takes a JSON array or one path per
line. Blank entries and repeats are dropped. The approval prompt lists the folders.

`askOnInstall` marks the values a person has to decide before the tool is first installed — an
account name, a folder. `required` fields are always asked. When the user clicks Install in
Roadie, the window asks for exactly these fields first. When an API or MCP client asks to
install, it may pass them in the request body (`POST /v1/tools/<name>/install` with
`{ "config": { "<key>": <value>, … } }`); the approval prompt shows what the client decided,
asks the user for anything still missing, and the user can change any of it before approving.
Secret (password) fields may be passed the same way; they are stored 0600 and never echoed
back by `GET /v1/requests/<id>`.

An install request may also name a registered `consumer`
(`{ "config": {…}, "consumer": "<id>" }`): the one approval then installs the tool *and* grants
that app its connection key, so it can read `/v1/tools/<name>/connection?consumer=<id>` as soon as
the request is `done`. Only for tools with a `connection`; the consumer must already be registered.

A `connection.webLogin` is the sign-in for the tool's own web page when the recipe put one
behind a login — a literal (slskd's web UI keeps slskd's own default: `{ "username": "slskd",
"password": "slskd" }`) or a generated secret (`"{secrets.webPassword}"`). It is expanded like
`url` and returned as `webLogin` alongside the key in `/v1/tools/<name>/connection` — to an
approved consumer and to the owner — so a person can open the page. It is never part of a tool's status. Both fields must be non-empty.

Two decisions belong to the engine rather than to a config field, and use reserved keys in the
same `config` object: `startNow` (start the daemon as soon as it is installed) and `autostart`
(start it at login). A recipe offers them with `startAfterInstall` / `autostart` at the top
level, each `{ "default": true|false, "askOnInstall": true|false }`; the prompt pre-ticks the
default and the user can flip it. Passing either key for a recipe that does not offer it is a
422. Roadie's own Start/Stop on the tool's card work regardless.

More engine decisions, all shown in the prompt with their defaults:

- `installDir`: any tool. It is the folder releases are unpacked into, instead of
  `<data>/tools/<name>/versions`. It must be an absolute path to a new or empty folder, because
  uninstalling removes it. It is fixed once installed: uninstall to move it.
- `ports.<name>`: a port the recipe offers with `askOnInstall`.
- `secrets.<key>`: a secret the recipe offers with `askOnInstall`, e.g. an API key the app
  already uses. Like passwords, it rides privately in the request and is never echoed back.
  Left out, Roadie generates it.

A blank value means the default. Any port or secret may be given at install; `askOnInstall`
only decides what Roadie's own prompt asks. A secret without `generate` is **required**: Roadie
refuses to install without it and names it in `missing`.

### For apps: offer the options before you install

1. Get the recipe. It is at
   `https://raw.githubusercontent.com/outcast1000/roadie-recipes/main/recipes/<name>.json`
   (`index.json` in the same place lists them), or Roadie already has it (`roadie tool status <name>`).
2. `roadie tool options <name | recipe.json>` (`GET /v1/tools/<name>/options`, or `POST` with
   `{"recipe": …}`) lists every value an install takes, in one shape:
   ```jsonc
   { "key": "ports.web", "label": "Web UI and API port", "kind": "port", "required": false,
     "secret": false, "default": 5030, "generated": false, "askOnInstall": true, "settable": true }
   ```
   `default` is expanded for this computer (`/Users/you/Music/Soulseek`, the real install
   folder). `otherInstance` is set when another copy of the tool is already running here, found
   by the recipe's `health` check on the ports the install would use: something answers but
   rejects Roadie's key. Its `message` says what to do, and `blocksStart` is true for a
   `singleton`. The install reply, the dry run and the approval prompt carry the same warning.
   Roadie never stops the other copy. `generated: true` means Roadie makes the value when none is given. Secrets never
   carry a value; an installed tool's options add `value` (or `set` for a secret). Show them to
   your user with the defaults filled in.
3. `roadie tool install <name | recipe.json> --set key=value …` (or `POST
   /v1/tools/<name>/install` with `{"config": {…}}`) with what the user chose. Leave out
   anything they kept at the default. Pass `startNow=false` if your app starts the tool itself.
   Bringing the file you downloaded is fine: identical to the catalog's, it counts as the
   catalog's.
4. The user approves once (a dialog, or Roadie's window). While it installs, `tool status` shows
   `installing: { phase, downloaded, total }`. The desktop request also has `progress`
   (`GET /v1/requests/<id>`). When it is done, `installed: true`.
5. `roadie tool start <name>`, then `roadie tool connection <name> --consumer <id>` for the URL
   and key.

Write `path` defaults with `/` (`{home}/Music/Tool`); on Windows the expanded default gets `\`.

`password` fields must be `secret: true` and go to `{secrets.key}`; everything else is
`{config.key}`. Patching a password with `""` clears it; omitting the key keeps it.

## FileDef and placeholders

```jsonc
{ "path": "{data}/tool.yml", "format": "yaml" | "json" | "env" | "ini" | "raw", "secret": true,
  "writeOnce": false,
  "content": { ... JSON tree with placeholders ... } }
```

`writeOnce: true` hands the file to the tool after install. Roadie writes it once, when it is
absent. After that, updates, starts and settings never rewrite it (the tool may edit it, e.g.
from its web UI), and ports and keys it carries no longer change. When every file is
`writeOnce`, the tool's settings in Roadie close after install (status `configurable: false`).
Uninstalling without keeping data removes the file.

`path` must start with `{data}`. The content is a JSON tree; Roadie expands placeholders and
serializes it in the named format, so quoting is by construction (a password full of `#`, `:`
and quotes cannot break a YAML file).

Placeholders, in any string: `{home}` `{data}` `{bin}` `{version}` `{platform.os}`
`{platform.arch}` `{ports.X}` `{config.X}` `{secrets.X}` `{connection.url}`. Filters:
`{ports.web|int}` emits a number, `{x|json}` a JSON-quoted string, `{x|shell}` a shell-quoted
string. A string that is exactly one placeholder keeps the value's type (a bool stays a bool).
`{{` and `}}` are literal braces.

Write paths with `/` (`"{config.downloadsDir}/.incomplete"`). On Windows, a string that
expands to an absolute path (`C:\…`, `\\server\…`) comes out normalized, as .NET's
`Path.GetFullPath` gives it. Every `/` becomes `\`, doubled and trailing separators go (a drive
root keeps its own, `D:\`), and `.` and `..` are resolved. So a downloads folder of `D:\` gives
`D:\.incomplete`, not `D:\\.incomplete`. Tools that insist a path is already normalized (slskd)
reject any other form. A path in the middle of a string (`--dir={data}/x`) is left as written.

Two directives inside `content`:

```jsonc
"api_keys": { "roadie": { "key": "{secrets.internalKey}" },      // static entries are kept
              "$each": "consumers", "key": "{item.id}",           // one entry per approved consumer
              "value": { "key": "{item.key}", "role": "readwrite" } }

"directories": { "$if": "config.shareDownloads", "then": ["{config.downloadsDir}"], "else": [] }
```

A `$if` without `else` that is false drops the key.

## ConfigEntry (`configuration`)

A `configuration` entry names a setting of the tool's own config file by its real key there, and
Roadie writes the value straight into the file the recipe generates:

```jsonc
"configuration": [
  { "entry": "shares.directories",   // dotted path inside the file — the tool's own setting name
    "label": "Also share these folders", "help": "...",
    "kind": "paths",                 // text | path | paths | bool | port (not password)
    "default": [], "askOnInstall": true, "required": false,
    "merge": "append",               // replace (default) | append (paths: added to the recipe's own list)
    "file": "{data}/slskd.yml" }     // which files[].path; optional when the recipe writes one file
]
```

- **The recipe decides what is open.** Anything not listed keeps the value the recipe's `files`
  wrote, so an app can set `shares.directories` but not the web binding or the API keys.
- It is a field like a `config` one, keyed by its entry path: stored under it, set under it
  (`--set shares.directories=…`, `{ "config": { "shares.directories": [...] } }`), asked on install
  and shown in the approval prompt the same way. It is not a `{config.…}` placeholder — the entry
  places itself.
- Roadie writes it after rendering the file: `replace` sets the key (creating missing levels),
  `append` adds a `paths` value's folders to the list already there, skipping repeats. No value
  stored leaves the file as the recipe wrote it.
- Only `yaml` and `json` files, which have nested keys. A password can't be an entry: declare a
  secret `config` field and place it with `{secrets.<key>}`.
- Use it for what an installing app has reason to decide. Settings Roadie must control (ports,
  bindings, keys, paths it owns) stay in `files`.

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

1. `GET /v1/recipes/schema` — this text and the catalog's slskd as an example.
2. `GET /v1/recipes/<closest>` — copy and edit.
3. `POST /v1/recipes/validate` until `ok: true`.
4. `PUT /v1/recipes/<name>` — saved as a draft; tell the user to review and Trust it in Roadie.
5. `POST /v1/recipes/<name>/dryrun` — resolves the release for this platform, checks the asset
   URL answers, and renders the files with placeholder values. Nothing is downloaded or written.
6. `POST /v1/tools/<name>/install` — a request the user approves; poll `GET /v1/requests/<id>`.
7. `GET /v1/recipes/<name>/submission` — once the user trusted it and it works, the file and a
   GitHub link for proposing it to the catalog. Open the pull request against
   `outcast1000/roadie-recipes` with your own GitHub access, or give the user the link. Roadie
   submits nothing. A change to a catalog recipe must raise `revision` above the catalog's.
