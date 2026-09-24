# Roadie MCP server

A dependency-free stdio [MCP](https://modelcontextprotocol.io) server that lets MCP clients —
Claude Desktop, Claude Code, Cursor, … — drive a running Roadie through its localhost API. It
is a translation layer: the Rust API owns every capability decision. Installing or removing a
tool is a *request* the user approves in Roadie's window; a recipe written through here is a
*draft* the user must Trust in Roadie before it can be installed. The bearer token and tool API
keys never reach the transcript unless `get_connection` is called with `includeSecret`.

Requires Node ≥ 18. No `npm install` — the script is self-contained.

## Setup

The script ships in the app bundle (`Resources/mcp/roadie-mcp.mjs` on macOS). Roadie →
Settings → **MCP** offers **Copy config** / **Copy command** with the absolute paths of the
script and of a `node` that can run it (GUI apps launch without the shell's PATH, so a bare
`"command": "node"` often resolves to nothing).

**Claude Code**

```bash
claude mcp add roadie -- /absolute/path/to/node "/Applications/Roadie.app/Contents/Resources/mcp/roadie-mcp.mjs"
```

**Claude Desktop** (`claude_desktop_config.json` → `mcpServers`)

```json
"roadie": {
  "command": "/absolute/path/to/node",
  "args": ["/Applications/Roadie.app/Contents/Resources/mcp/roadie-mcp.mjs"]
}
```

Roadie does not need to be running when the client starts — tools answer with a pointer until it
is, and `launch_app` starts the installed app and waits for its API.

The server finds Roadie through `roadie-api.json` in the app's data directory
(`~/Library/Application Support/com.outcast1000.roadie` on macOS, `%APPDATA%\com.outcast1000.roadie`
on Windows, `~/.local/share/com.outcast1000.roadie` on Linux). Pass `--data-dir=<dir>` to override.

## Tools

Status and control: `list_tools`, `tool_status`, `start_tool`, `stop_tool`, `restart_tool`,
`update_tool`, `set_autostart`, `configure_tool`, `tool_logs`, `get_connection`, `launch_app`.

Requests (user approves in Roadie): `install_tool`, `uninstall_tool`, `request_status`.

Recipe authoring: `recipe_schema`, `list_recipes`, `get_recipe`, `validate_recipe`,
`write_recipe` (draft), `dryrun_recipe`, `delete_recipe`.

## Tests

```bash
node --test mcp/
```
