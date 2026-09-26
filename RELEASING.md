# Releasing Roadie

Roadie ships as two independent releases from this one repository. Both are built by
`.github/workflows/release.yml`, which runs when a tag is pushed.

| Tag | What gets published | Marked "latest" |
|---|---|---|
| `desktop-v<version>` | The desktop app (window, service, API, MCP): `.dmg` and `.app.tar.gz` for macOS on Apple silicon and Intel, an NSIS installer for Windows x64, and `latest.json` for the app's self-updater | yes |
| `cli-v<version>` | The standalone CLI: `roadie-cli-<version>-<platform>.tar.gz` for macOS or `.zip` for Windows, plus `SHA256SUMS` and `manifest.json` | no |

Only desktop releases are marked "latest". The desktop updater reads
`releases/latest/download/latest.json`, so a CLI release must never become "latest".

## Cutting a release

Both releases are built from the same crate, so they share one version number. It lives in five
files, and `scripts/version.mjs` keeps them in step.

```bash
node scripts/version.mjs              # show what each file says
node scripts/version.mjs 0.2.0        # set it everywhere
git commit -am "Roadie 0.2.0"
git tag cli-v0.2.0                     # and/or desktop-v0.2.0
git push origin main cli-v0.2.0
```

The workflow first checks that every version file matches the tag, and refuses otherwise. It then
runs every test suite on macOS and Windows, builds the release, and publishes it. Releasing the
CLI and the desktop app at different versions is fine. Each release line simply skips the
versions it didn't release.

To rebuild an existing tag, for example after fixing the workflow, run **Actions → Release → Run
workflow** with that tag. Its assets are replaced in place.

## One-time setup for desktop releases

The desktop app updates itself and only accepts updates signed with Roadie's key. The workflow
refuses a desktop release until both of these are set:

1. Generate the key pair once, and keep the private key safe. It cannot be recovered, and a lost
   key means installed apps can never update again.

   ```bash
   npx tauri signer generate -w ~/.tauri/roadie.key
   ```

2. Paste the printed public key into `plugins.updater.pubkey` in `src-tauri/tauri.conf.json` and
   commit it.
3. In the GitHub repository settings, under **Secrets and variables → Actions**, add
   `TAURI_SIGNING_PRIVATE_KEY` (the contents of `~/.tauri/roadie.key`). If you chose a
   password, also add `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

CLI releases need neither.

## What apps bundling the CLI get

Each CLI release carries a `manifest.json`:

```json
{
  "name": "roadie-cli",
  "version": "0.2.0",
  "tag": "cli-v0.2.0",
  "assets": {
    "darwin-arm64": { "file": "roadie-cli-0.2.0-darwin-arm64.tar.gz", "url": "https://github.com/outcast1000/roadie/releases/download/cli-v0.2.0/roadie-cli-0.2.0-darwin-arm64.tar.gz", "sha256": "…", "binary": "roadie" },
    "darwin-x64": { … },
    "windows-x64": { "file": "roadie-cli-0.2.0-windows-x64.zip", …, "binary": "roadie.exe" }
  }
}
```

The platform keys are Roadie's own recipe platform keys. An app downloads the archive for its
platform, checks the `sha256`, and ships the binary. It can do that two ways:

- **Follow the newest CLI.** Every CLI release also copies its `manifest.json` to a fixed
  address, the `cli-latest` release:
  `https://github.com/outcast1000/roadie/releases/download/cli-latest/manifest.json`.
  `cli-latest` is a prerelease that is never marked "latest" (that stays the desktop app's), holds
  nothing but that manifest, and only ever moves forward — rebuilding an older tag leaves it
  alone. Its URLs point into the versioned `cli-v*` release. Viboplr reads it this way.
- **Pin a tag.** Download from `releases/download/cli-v<version>/` and move on by pinning a newer
  tag.

Following is safe only because **the CLI's output is additive**: a release may add commands,
flags and JSON fields, but never renames or removes one, or changes what an existing field
means. A change that can't be made that way needs a new command next to the old one.

The bundled binary reports its own version:

```bash
roadie version      # {"version": "0.2.0", "release": "cli"}
```

## Known gaps

- **Code signing.** macOS builds are not signed with a Developer ID or notarized, and Windows
  builds are not Authenticode-signed. Gatekeeper and SmartScreen will warn on first open. The
  updater signature above is separate: it proves an update came from Roadie, but does not satisfy
  the OS.
- **Private repository.** Release downloads from a private repository need a GitHub token. That
  breaks the desktop updater and anonymous CLI downloads until the repository, or a separate
  public releases repository, is public.
