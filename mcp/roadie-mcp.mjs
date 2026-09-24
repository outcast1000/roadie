#!/usr/bin/env node
// Roadie MCP server — a dependency-free stdio MCP server that lets MCP clients
// (Claude Desktop, Claude Code, Cursor, …) drive a running Roadie through its
// localhost API. It is a translation layer: the Rust API owns every capability
// decision (install/uninstall are *requests* the user approves in Roadie;
// drafts must be Trusted in Roadie). This server discovers the app, holds the
// bearer token, and presents typed tools. The token and any tool API key never
// appear in tool results unless a tool is asked for one explicitly.
//
// Requires Node ≥ 18. No npm install.
//
//   claude mcp add roadie -- /abs/path/to/node /path/to/roadie-mcp.mjs
//
// Options: --data-dir=<dir> (where roadie-api.json lives; default per OS).

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";

const IDENTIFIER = "com.outcast1000.roadie";
const DEFAULT_PORT = 47630;
const PORT_RANGE = 10;
const CLIENT_HEADER = "MCP client";

// --- Discovery -----------------------------------------------------------

export function defaultDataDir(platform = process.platform, home = os.homedir(), env = process.env) {
  // Join with the target platform's separator, not the host's, so the answer is the same wherever it is computed.
  const p = platform === "win32" ? path.win32 : path.posix;
  if (platform === "darwin") return p.join(home, "Library", "Application Support", IDENTIFIER);
  if (platform === "win32") return p.join(env.APPDATA || p.join(home, "AppData", "Roaming"), IDENTIFIER);
  return p.join(env.XDG_DATA_HOME || p.join(home, ".local", "share"), IDENTIFIER);
}

function argValue(name) {
  const hit = process.argv.find((a) => a.startsWith(`--${name}=`));
  return hit ? hit.slice(name.length + 3) : undefined;
}

export function readDiscovery(dataDir) {
  try {
    const j = JSON.parse(fs.readFileSync(path.join(dataDir, "roadie-api.json"), "utf8"));
    if (typeof j.port !== "number" || typeof j.token !== "string") return null;
    return { port: j.port, token: j.token, version: j.version };
  } catch {
    return null;
  }
}

export async function probeHealth(port, fetchImpl = fetch) {
  try {
    const ac = new AbortController();
    const t = setTimeout(() => ac.abort(), 1500);
    const r = await fetchImpl(`http://127.0.0.1:${port}/v1/health`, { signal: ac.signal });
    clearTimeout(t);
    if (!r.ok) return null;
    const j = await r.json();
    return j && j.app === "roadie" ? j : null;
  } catch {
    return null;
  }
}

/** Find a live Roadie: discovery file first, then the port range. */
export async function discover(dataDir, fetchImpl = fetch) {
  const disc = readDiscovery(dataDir);
  if (disc && (await probeHealth(disc.port, fetchImpl))) return disc;
  for (let p = DEFAULT_PORT; p < DEFAULT_PORT + PORT_RANGE; p++) {
    if (await probeHealth(p, fetchImpl)) {
      // Running, but the discovery file is stale or elsewhere: token unknown.
      return { port: p, token: disc?.token ?? null, version: null };
    }
  }
  return null;
}

// --- API client ----------------------------------------------------------

class Api {
  constructor(dataDir, fetchImpl = fetch) {
    this.dataDir = dataDir;
    this.fetch = fetchImpl;
    this.conn = null;
  }
  async connect() {
    if (this.conn && (await probeHealth(this.conn.port, this.fetch))) return this.conn;
    this.conn = await discover(this.dataDir, this.fetch);
    return this.conn;
  }
  async call(method, route, body, { auth = true, query } = {}) {
    const conn = await this.connect();
    if (!conn) throw new NotRunning();
    if (auth && !conn.token) throw new Error(`Roadie is running on port ${conn.port} but its token could not be read from ${path.join(this.dataDir, "roadie-api.json")}. Pass --data-dir=<Roadie data dir>.`);
    const url = new URL(`http://127.0.0.1:${conn.port}${route}`);
    for (const [k, v] of Object.entries(query || {})) if (v !== undefined && v !== null) url.searchParams.set(k, String(v));
    const headers = { "X-Roadie-Client": CLIENT_HEADER };
    if (auth) headers.Authorization = `Bearer ${conn.token}`;
    if (body !== undefined) headers["Content-Type"] = "application/json";
    const r = await this.fetch(url, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = { raw: text }; }
    if (!r.ok) {
      const err = new Error(`${method} ${route} → HTTP ${r.status}${json?.error ? `: ${json.error}` : ""}`);
      err.status = r.status;
      err.body = json;
      throw err;
    }
    return json;
  }
}

class NotRunning extends Error {
  constructor() {
    super("Roadie is not running (its local API did not answer). Ask the user to open Roadie, or call launch_app.");
  }
}

// --- Redaction -----------------------------------------------------------

/** Never let a key or token into a transcript unless a tool asked for it. */
export function redact(value, keep = false) {
  if (keep) return value;
  if (Array.isArray(value)) return value.map((v) => redact(v));
  if (value && typeof value === "object") {
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      if (/^(apiKey|token|key|password|secret)$/i.test(k) && typeof v === "string" && v.length > 0) out[k] = "<redacted>";
      else out[k] = redact(v);
    }
    return out;
  }
  return value;
}

// --- Launch --------------------------------------------------------------

export function launchCommand(platform = process.platform) {
  if (platform === "darwin") return ["open", ["-a", "Roadie"]];
  if (platform === "win32") return ["cmd", ["/c", "start", "", "roadie://open"]];
  return ["xdg-open", ["roadie://open"]];
}

async function launchApp(api) {
  const [cmd, args] = launchCommand();
  await new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { stdio: "ignore", detached: true });
    child.on("error", reject);
    child.on("exit", (code) => (code === 0 ? resolve() : reject(new Error(`${cmd} exited with ${code}`))));
    child.unref();
  });
  for (let i = 0; i < 40; i++) {
    await new Promise((r) => setTimeout(r, 500));
    if (await api.connect()) return await api.call("GET", "/v1/health", undefined, { auth: false });
  }
  throw new Error("Roadie was launched but its API did not answer within 20s.");
}

// --- Tools ---------------------------------------------------------------

const str = (description) => ({ type: "string", description });
const bool = (description) => ({ type: "boolean", description });

export const TOOLS = [
  { name: "list_tools", description: "Every tool Roadie knows (built-in and user recipes, incl. drafts): installed, version, running/healthy, update available, connection policy, approved consumers. Read-only.", inputSchema: { type: "object", properties: {} } },
  { name: "tool_status", description: "Full status of one tool by name.", inputSchema: { type: "object", properties: { name: str("Tool name, e.g. slskd") }, required: ["name"] } },
  { name: "start_tool", description: "Start an installed daemon. Fails if it is a cli tool or not installed.", inputSchema: { type: "object", properties: { name: str("Tool name") }, required: ["name"] } },
  { name: "stop_tool", description: "Stop a running daemon gracefully (API route, then signal).", inputSchema: { type: "object", properties: { name: str("Tool name") }, required: ["name"] } },
  { name: "restart_tool", description: "Stop then start a daemon.", inputSchema: { type: "object", properties: { name: str("Tool name") }, required: ["name"] } },
  { name: "update_tool", description: "Fetch and stage the latest release of an installed tool; applied at once when the daemon is stopped or idle, otherwise deferred (see updateDeferredReason). With `recipe` (the recipe your app ships) that differs from the one Roadie trusts, nothing runs yet: it returns a requestId for the user to review and trust the new recipe, and approving updates the tool. Poll request_status.", inputSchema: { type: "object", properties: { name: str("Tool name"), recipe: { type: "object", description: "Optional: the full recipe your app ships for this tool (same name). Identical to the trusted one: a plain update.", additionalProperties: true } }, required: ["name"] } },
  { name: "set_autostart", description: "Start a daemon at login (a per-user login item), or not.", inputSchema: { type: "object", properties: { name: str("Tool name"), enabled: bool("true to start at login") }, required: ["name", "enabled"] } },
  { name: "configure_tool", description: "Set non-secret config fields (see tool_status → config and the recipe's config list). Secret fields (passwords) must be entered by the user in Roadie. A running daemon is restarted when idle, else marked restartPending.", inputSchema: { type: "object", properties: { name: str("Tool name"), patch: { type: "object", description: "Field key → value", additionalProperties: true } }, required: ["name", "patch"] } },
  { name: "tool_logs", description: "Last lines of a daemon's stdout/stderr log.", inputSchema: { type: "object", properties: { name: str("Tool name"), lines: { type: "integer", description: "How many lines (default 100, max 2000)" } }, required: ["name"] } },
  { name: "get_connection", description: "A daemon's base URL (and, only with includeSecret=true, its API key) so you can configure another client. Without includeSecret the key is redacted — prefer telling the user where to approve the consuming app in Roadie.", inputSchema: { type: "object", properties: { name: str("Tool name"), includeSecret: bool("Return the API key in clear (it will be in the transcript)") }, required: ["name"] } },
  { name: "install_tool", description: "Ask to install a tool. Nothing installs by itself: this queues a request the user approves in Roadie (its window, or a native dialog when Roadie runs without one). Recipes mark the decisions an install needs (config fields with askOnInstall/required — see get_recipe); pass the ones you and the user have settled in `config`, the prompt shows them and asks the user for the rest. Where Roadie has no window (health `approvalSurface` is not \"window\"), nothing can ask, so a missing `required` value fails the call with 422 and a `missing` list. Returns the request id and a `decisions` list saying which are settled; poll request_status until done/failed.", inputSchema: { type: "object", properties: { name: str("Tool name (must be a trusted recipe)"), config: { type: "object", description: "Install-time decisions: field key → value (e.g. { soulseekUsername: \"…\" }). Passwords are allowed but will sit in this transcript; prefer letting the user type them in Roadie. Daemons that offer them also take the reserved booleans startNow (start right after installing) and autostart (start at login).", additionalProperties: true }, consumer: str("A registered consumer id (see list/register consumers). One approval then installs the tool AND grants that app its connection key, so it can call get_connection right after."), recipe: { type: "object", description: "Optional: the full recipe your app ships for this tool (same name). When Roadie has no such recipe or a different one, it rides in the request: the user reviews it in the prompt, and approving trusts it and installs (recipeChange in the reply says new/replacesBuiltin/changesTrusted/replacesDraft). Identical to the trusted recipe: an ordinary install. Validate it first with validate_recipe.", additionalProperties: true } }, required: ["name"] } },
  { name: "uninstall_tool", description: "Ask to remove a tool (a request the user approves in Roadie).", inputSchema: { type: "object", properties: { name: str("Tool name"), keepData: bool("Keep its config and state (default false)") }, required: ["name"] } },
  { name: "request_status", description: "Status of an install/uninstall/connect request: pending, approved, declined, done or failed, with install progress while running.", inputSchema: { type: "object", properties: { id: str("Request id") }, required: ["id"] } },
  { name: "list_requests", description: "Requests still waiting for the user's answer in Roadie.", inputSchema: { type: "object", properties: {} } },
  { name: "recipe_schema", description: "The recipe format documentation (SCHEMA.md) plus a complete built-in example. Read this before writing a recipe.", inputSchema: { type: "object", properties: {} } },
  { name: "list_recipes", description: "All recipes with author, revision, target platforms, origin (builtin/user/draft) and whether they are trusted.", inputSchema: { type: "object", properties: {} } },
  { name: "get_recipe", description: "One recipe's full JSON — copy the closest one when writing a new recipe.", inputSchema: { type: "object", properties: { name: str("Recipe name") }, required: ["name"] } },
  { name: "validate_recipe", description: "Validate a recipe document without saving. Errors carry JSON pointers to the offending field.", inputSchema: { type: "object", properties: { recipe: { type: "object", description: "The recipe JSON", additionalProperties: true } }, required: ["recipe"] } },
  { name: "write_recipe", description: "Save a recipe as a DRAFT. It cannot be installed until the user reviews and Trusts it in Roadie — tell them so. Re-submitting a draft replaces it; built-in and trusted names are refused.", inputSchema: { type: "object", properties: { recipe: { type: "object", description: "The recipe JSON", additionalProperties: true } }, required: ["recipe"] } },
  { name: "dryrun_recipe", description: "Resolve the latest release for this computer, check the download URL answers, and render the config files with placeholder values. Downloads and writes nothing. Pass either a saved recipe name or an inline recipe.", inputSchema: { type: "object", properties: { name: str("Saved recipe name"), recipe: { type: "object", description: "Inline recipe (optional; name is then taken from it)", additionalProperties: true } } } },
  { name: "delete_recipe", description: "Delete a draft or user recipe (not built-ins; not while its tool is installed).", inputSchema: { type: "object", properties: { name: str("Recipe name") }, required: ["name"] } },
  { name: "launch_app", description: "Start Roadie when its API is not answering (opens the app, whose window starts the background service), and wait for the API. With \"Run in the background\" on, the service normally runs at login without this.", inputSchema: { type: "object", properties: {} } },
];

export async function callTool(api, name, args = {}) {
  const need = (k) => {
    if (args[k] === undefined || args[k] === null || args[k] === "") throw new Error(`${k} is required`);
    return args[k];
  };
  switch (name) {
    case "list_tools": return redact(await api.call("GET", "/v1/tools", undefined, { auth: false }));
    case "tool_status": return redact(await api.call("GET", `/v1/tools/${encodeURIComponent(need("name"))}`, undefined, { auth: false }));
    case "start_tool": return redact(await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/start`));
    case "stop_tool": return redact(await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/stop`));
    case "restart_tool": return redact(await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/restart`));
    case "update_tool": {
      const recipe = args.recipe && typeof args.recipe === "object" ? args.recipe : undefined;
      const r = await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/update`, recipe ? { recipe } : undefined);
      if (r && r.requestId) return { ...r, next: "The recipe differs from the one Roadie trusts. The user must review and approve it in Roadie; approving updates the tool. Poll request_status with this requestId." };
      return redact(r);
    }
    case "set_autostart": return redact(await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/autostart`, { enabled: !!need("enabled") }));
    case "configure_tool": return redact(await api.call("PATCH", `/v1/tools/${encodeURIComponent(need("name"))}/config`, { patch: need("patch") }));
    case "tool_logs": return await api.call("GET", `/v1/tools/${encodeURIComponent(need("name"))}/logs`, undefined, { query: { lines: args.lines } });
    case "get_connection": {
      const c = await api.call("GET", `/v1/tools/${encodeURIComponent(need("name"))}/connection`);
      return redact(c, !!args.includeSecret);
    }
    case "install_tool": {
      const body = {};
      if (args.config && typeof args.config === "object") body.config = args.config;
      if (typeof args.consumer === "string" && args.consumer) body.consumer = args.consumer;
      if (args.recipe && typeof args.recipe === "object") body.recipe = args.recipe;
      const r = await api.call("POST", `/v1/tools/${encodeURIComponent(need("name"))}/install`, Object.keys(body).length ? body : undefined);
      const open = Array.isArray(r.decisions) ? r.decisions.filter((d) => !d.settled).map((d) => d.label) : [];
      const review = r.recipeChange ? " It includes the recipe you sent, which the user reviews and trusts with the same click." : "";
      return { ...r, next: `The user must approve this in Roadie${open.length ? ` and will be asked for: ${open.join(", ")}` : ""}.${review} Poll request_status with this requestId; installation progress appears there.` };
    }
    case "uninstall_tool": {
      const r = await api.call("DELETE", `/v1/tools/${encodeURIComponent(need("name"))}`, undefined, { query: { keepData: args.keepData ? "true" : undefined } });
      return { ...r, next: "The user must approve this in Roadie. Poll request_status." };
    }
    case "request_status": return await api.call("GET", `/v1/requests/${encodeURIComponent(need("id"))}`);
    case "list_requests": return await api.call("GET", "/v1/requests");
    case "recipe_schema": return await api.call("GET", "/v1/recipes/schema");
    case "list_recipes": return await api.call("GET", "/v1/recipes");
    case "get_recipe": return await api.call("GET", `/v1/recipes/${encodeURIComponent(need("name"))}`);
    case "validate_recipe": return await api.call("POST", "/v1/recipes/validate", { recipe: need("recipe") });
    case "write_recipe": {
      const recipe = need("recipe");
      if (!recipe.name) throw new Error("recipe.name is required");
      const r = await api.call("PUT", `/v1/recipes/${encodeURIComponent(recipe.name)}`, { recipe });
      return { ...r, next: "Saved as a draft. Ask the user to open Roadie, read the review (source, run arguments, files written) and click Trust. Then dryrun_recipe, then install_tool." };
    }
    case "dryrun_recipe": {
      const inline = args.recipe;
      const n = inline?.name ?? need("name");
      return await api.call("POST", `/v1/recipes/${encodeURIComponent(n)}/dryrun`, inline ? { recipe: inline } : undefined);
    }
    case "delete_recipe": await api.call("DELETE", `/v1/recipes/${encodeURIComponent(need("name"))}`); return { deleted: true };
    case "launch_app": return await launchApp(api);
    default: throw new Error(`unknown tool ${name}`);
  }
}

// --- MCP stdio transport (JSON-RPC 2.0, newline-delimited) ----------------

const SERVER_INFO = { name: "roadie", version: "0.1.0" };
const INSTRUCTIONS = `Roadie installs, configures, runs and updates the tools other apps need (slskd, yt-dlp, ffmpeg, …) from declarative recipes.
Rules the API enforces: nothing installs without the user's click — install_tool/uninstall_tool queue a request the user approves in Roadie; write_recipe saves a DRAFT the user must Trust in Roadie before install. Secrets (passwords, API keys) are entered/approved in Roadie, never through here; results redact keys unless get_connection is called with includeSecret.
Authoring a recipe: recipe_schema → get_recipe(closest built-in) → edit → validate_recipe → write_recipe → ask the user to Trust it → dryrun_recipe → install_tool → request_status.
An app that ships its own recipe skips the draft: install_tool/update_tool with { recipe } put it in the request, and the user reviews and trusts it in the same prompt that approves the install.`;

export function handleMessage(msg, ctx) {
  const { id, method, params } = msg;
  const reply = (result) => ({ jsonrpc: "2.0", id, result });
  const fail = (code, message, data) => ({ jsonrpc: "2.0", id, error: { code, message, data } });
  switch (method) {
    case "initialize":
      return Promise.resolve(reply({ protocolVersion: params?.protocolVersion || "2024-11-05", capabilities: { tools: {} }, serverInfo: SERVER_INFO, instructions: INSTRUCTIONS }));
    case "notifications/initialized":
    case "notifications/cancelled":
      return Promise.resolve(null);
    case "ping":
      return Promise.resolve(reply({}));
    case "tools/list":
      return Promise.resolve(reply({ tools: TOOLS }));
    case "tools/call": {
      const name = params?.name;
      if (!TOOLS.some((t) => t.name === name)) return Promise.resolve(fail(-32602, `unknown tool ${name}`));
      return callTool(ctx.api, name, params?.arguments || {})
        .then((result) => reply({ content: [{ type: "text", text: JSON.stringify(result, null, 2) }] }))
        .catch((e) => reply({ isError: true, content: [{ type: "text", text: e.message + (e.body?.errors ? "\n" + JSON.stringify(e.body.errors, null, 2) : "") }] }));
    }
    default:
      if (id === undefined) return Promise.resolve(null);
      return Promise.resolve(fail(-32601, `method not found: ${method}`));
  }
}

export function serve(api, input = process.stdin, output = process.stdout) {
  let buf = "";
  const write = (obj) => obj && output.write(JSON.stringify(obj) + "\n");
  input.setEncoding("utf8");
  input.on("data", (chunk) => {
    buf += chunk;
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { write({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }); continue; }
      handleMessage(msg, { api }).then(write).catch((e) => write({ jsonrpc: "2.0", id: msg.id ?? null, error: { code: -32000, message: e.message } }));
    }
  });
  input.on("end", () => process.exit(0));
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === path.resolve(new URL(import.meta.url).pathname);
if (isMain) {
  const dataDir = argValue("data-dir") || defaultDataDir();
  serve(new Api(dataDir));
}

export { Api, NotRunning };
