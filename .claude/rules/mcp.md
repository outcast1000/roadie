---
paths:
  - "mcp/**"
---

# MCP server (mcp/roadie-mcp.mjs)

One dependency-free Node ≥ 18 ES module, bundled into the app (`bundle.resources` →
`Resources/mcp/roadie-mcp.mjs`) so it is version-matched with the Rust API. It is a translation
layer: **every capability decision is the API's** (install = request, recipe = draft, secrets
gated). Do not add logic here that the API does not enforce.

- **Discovery**: `roadie-api.json` in the per-OS data dir (`--data-dir=` overrides), then a
  `/v1/health` probe; falls back to scanning 47630–47639 if the recorded port is dead (token then
  unknown → tier-2 calls explain what to pass).
- **Redaction**: `redact()` masks `apiKey|token|key|password|secret` in every result unless the
  tool was called with `includeSecret: true` (only `get_connection` offers it). The bearer token
  never appears anywhere.
- **Identity**: every request carries `X-Roadie-Client: MCP client`, which is what the user
  sees on approval prompts.
- **Errors**: HTTP failures become `isError` results with the API's message and, for recipe
  validation, the `errors[]` pointers — that is how an assistant fixes a recipe.
- **Instructions** (`INSTRUCTIONS`) teach the authoring loop: `recipe_schema` → `get_recipe`
  → `validate_recipe` → `write_recipe` → user trusts → `dryrun_recipe` → `install_tool` →
  `request_status`. Keep it in step with SCHEMA.md and the tool list.
- Adding a tool = an entry in `TOOLS` (JSON schema for inputs) + a `case` in `callTool` + a
  test in `roadie-mcp.test.mjs` using the fake `fetch` (no running Roadie needed).
- Framing is newline-delimited JSON-RPC 2.0 over stdio; `initialize`, `notifications/*`,
  `ping`, `tools/list`, `tools/call`. Nothing else.
