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

Recipes come from the [Roadie recipe catalog](https://github.com/outcast1000/roadie-recipes):
[slskd](https://github.com/slskd/slskd) (Soulseek), yt-dlp and ffmpeg so far; rqbit and
cloudflared are planned. Anyone can add or fix one there with a pull request, without a Roadie
release.

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
- **CLI:** the same binary is a client. It starts the background service if none answers, prints
  one JSON document, and never approves anything itself:

  ```bash
  roadie tool status slskd                      # installed? running? (no service needed to be up)
  roadie tool start slskd
  roadie tool install slskd --set soulseekUsername=bj --consumer viboplr --wait
                                                # opens Roadie for your approval; one click installs
                                                #   and grants viboplr its key;
                                                #   exit 0 done · 1 failed · 2 you declined · 3 error
  roadie request <id> --wait                    # follow a request created earlier
  roadie --as "My Player" tool install slskd    # the prompt names your app, not "roadie CLI"
  roadie service status|stop
  ```

  On macOS the binary is `Roadie.app/Contents/MacOS/roadie`. Nothing needs to be running
  beforehand: with "Run in the background" off, the service the CLI starts exits again after a few
  idle minutes.

## Recipes

The format is documented in [`recipes/SCHEMA.md`](recipes/SCHEMA.md). Roadie ships no recipes: it
reads them from the catalog, [`outcast1000/roadie-recipes`](https://github.com/outcast1000/roadie-recipes),
refreshing every few hours (`roadie catalog refresh` or **Refresh catalog** to do it now).

The catalog is not signed, so Roadie never trusts a recipe just because it is there. The first
time you install a tool, Roadie shows you its recipe: where it downloads from, what it runs and
which files it writes. You approve it with the install. When the catalog publishes a new revision
of a recipe you trusted, Roadie offers it as a **recipe update** for the same review, and never
applies it silently. A tool whose recipe leaves the catalog keeps working with the copy you
approved.

A recipe submitted through the API is a **draft**. It becomes installable only when you click
**Trust**. To share one you wrote, use **Submit to catalog…** in its review. It opens GitHub with
the file filled in, and you open the pull request from your own account. Roadie holds no GitHub
credential. Assistants can do the same with the MCP tool `submit_recipe`.

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
`%APPDATA%\com.outcast1000.roadie` (Windows). Roadie runs on macOS and Windows only.

## License

GPL-3.0-or-later.
