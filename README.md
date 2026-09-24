# Roadie

Roadie installs, configures, runs and updates the command-line tools and background services
other apps depend on — from **recipes**, one JSON file per tool. It is the crew member who sets up
the gear so the band can play.

Nothing installs without your click. Apps and AI assistants can *ask*; you approve in Roadie.

## What it does

- **Install** a tool's latest release for your computer, verify it (checksums when upstream
  publishes them, always by running the binary's version flag), and keep versions side by side.
- **Configure** it through a form the recipe describes; secrets stay in Roadie's data dir with
  `0600` permissions and are rendered into the tool's own config file.
- **Run** daemons detached from Roadie — they keep running when Roadie quits — bound to
  `127.0.0.1`, with a graceful stop ladder (the tool's own API, then a signal, then a kill), and
  an optional login item.
- **Update** daily; a busy daemon is never restarted for an update.
- **Share** a daemon's connection with another app after you approve it once; each app gets its
  own key, revocable from the tool's card.

Roadie runs as a small **background service** plus a window. The service owns the API, the
tools and the daily updates and, with "Run in the background" on (the default), starts at login
and keeps going when the window is closed. Turn it off in Settings and Roadie behaves like a
plain app: the service exits shortly after you close the window and nothing starts at login.
Either way the window is the only thing that can approve an install: it proves itself to the
service over a local channel other programs cannot use.

First recipe: [slskd](https://github.com/slskd/slskd) (Soulseek). Planned: yt-dlp, ffmpeg,
rqbit, cloudflared.

## For other apps

- **Deep links:** `roadie://install/<tool>?consumer=<id>&return=<url>`, `roadie://connect/…`,
  `roadie://open/<tool>`. Roadie focuses the tool, and after you approve the connection it opens
  the return link with `?status=connected&tool=<tool>`; the app then reads
  `GET /v1/tools/<tool>/connection?consumer=<id>`.
- **Local API:** `http://127.0.0.1:47630` (falls back through 47639). Read-only status needs no
  token; control, recipe authoring and *requests* need the bearer token from `roadie-api.json`
  in Roadie's data directory. Install and uninstall over the API are requests you approve in the
  window. Full route list in `src-tauri/src/api/mod.rs`.
- **MCP:** a dependency-free stdio server ships in the bundle — see [`mcp/README.md`](mcp/README.md).
  Settings → MCP copies a ready client config.

## Recipes

The format is documented in [`recipes/SCHEMA.md`](recipes/SCHEMA.md). A recipe submitted through
the API is a **draft**: Roadie shows where it downloads from, what it runs and which files it
writes; it becomes installable only when you click **Trust**.

## Development

```bash
npm install --legacy-peer-deps      # npm 10.9 trips over a peer set otherwise
npm run tauri dev                   # window + API on 127.0.0.1:47630
cd src-tauri && cargo test          # engine, recipe validator, API router, owner channel, events
./src-tauri/target/debug/roadie --serve   # the service alone, headless
npm run test:mcp                    # MCP server
cd src-tauri && cargo test --lib tools::probe -- --ignored --nocapture   # real install/start/stop of slskd
```

Data lives in `~/Library/Application Support/com.outcast1000.roadie` (macOS),
`%APPDATA%\com.outcast1000.roadie` (Windows), `~/.local/share/com.outcast1000.roadie` (Linux).

## License

GPL-3.0-or-later.
