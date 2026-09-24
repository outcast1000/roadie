import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Api, TOOLS, callTool, defaultDataDir, discover, handleMessage, readDiscovery, redact, launchCommand } from "./roadie-mcp.mjs";

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "roadie-mcp-"));

/** A fetch stub that answers health on `port` and records calls. */
function fakeFetch(port, routes = {}) {
  const calls = [];
  const f = async (input, init = {}) => {
    const url = new URL(String(input));
    calls.push({ url: url.pathname + url.search, method: init.method || "GET", headers: init.headers || {}, body: init.body });
    if (url.port !== String(port)) throw new Error("ECONNREFUSED");
    if (url.pathname === "/v1/health") return new Response(JSON.stringify({ app: "roadie", version: "t", apiVersion: 1 }), { status: 200 });
    const hit = routes[`${init.method || "GET"} ${url.pathname}`];
    if (!hit) return new Response(JSON.stringify({ error: "nope" }), { status: 404 });
    const [status, body] = typeof hit === "function" ? hit(init) : hit;
    return new Response(JSON.stringify(body), { status });
  };
  f.calls = calls;
  return f;
}

test("default data dir follows the OS conventions", () => {
  assert.equal(defaultDataDir("darwin", "/Users/a", {}), "/Users/a/Library/Application Support/com.outcast1000.roadie");
  assert.equal(defaultDataDir("win32", "C:\\Users\\a", { APPDATA: "C:\\Users\\a\\AppData\\Roaming" }), "C:\\Users\\a\\AppData\\Roaming\\com.outcast1000.roadie");
  assert.equal(defaultDataDir("linux", "/home/a", {}), "/home/a/.local/share/com.outcast1000.roadie");
});

test("discovery: missing file, stale file, good file", async () => {
  const dir = tmp();
  assert.equal(readDiscovery(dir), null);
  assert.equal(await discover(dir, fakeFetch(59999)), null, "nothing listening anywhere in range");

  fs.writeFileSync(path.join(dir, "roadie-api.json"), JSON.stringify({ port: 47635, token: "t".repeat(64) }));
  const stale = await discover(dir, fakeFetch(47631));
  assert.equal(stale.port, 47631, "falls back to the range scan when the recorded port is dead");
  assert.equal(stale.token, "t".repeat(64), "keeps the recorded token");

  const good = await discover(dir, fakeFetch(47635));
  assert.equal(good.port, 47635);
});

test("tool results redact keys unless asked", () => {
  const v = { url: "http://x", apiKey: "secret", nested: [{ token: "abc", name: "n" }] };
  assert.deepEqual(redact(v), { url: "http://x", apiKey: "<redacted>", nested: [{ token: "<redacted>", name: "n" }] });
  assert.deepEqual(redact(v, true), v);
});

test("install_tool sends the client header and returns the approval hint", async () => {
  const dir = tmp();
  fs.writeFileSync(path.join(dir, "roadie-api.json"), JSON.stringify({ port: 47630, token: "a".repeat(64) }));
  const f = fakeFetch(47630, { "POST /v1/tools/slskd/install": [202, { requestId: "r1", status: "pending" }] });
  const api = new Api(dir, f);
  const out = await callTool(api, "install_tool", { name: "slskd" });
  assert.equal(out.requestId, "r1");
  assert.match(out.next, /approve/);
  const call = f.calls.find((c) => c.url === "/v1/tools/slskd/install");
  assert.equal(call.headers.Authorization, `Bearer ${"a".repeat(64)}`);
  assert.equal(call.headers["X-Roadie-Client"], "MCP client");
});

test("write_recipe forwards validation errors with pointers", async () => {
  const dir = tmp();
  fs.writeFileSync(path.join(dir, "roadie-api.json"), JSON.stringify({ port: 47630, token: "a".repeat(64) }));
  const f = fakeFetch(47630, { "PUT /v1/recipes/demo": [422, { error: "recipe is invalid", errors: [{ pointer: "/version/regex", message: "needs a capture" }] }] });
  const api = new Api(dir, f);
  const reply = await handleMessage({ jsonrpc: "2.0", id: 7, method: "tools/call", params: { name: "write_recipe", arguments: { recipe: { name: "demo" } } } }, { api });
  assert.equal(reply.result.isError, true);
  assert.match(reply.result.content[0].text, /\/version\/regex/);
});

test("get_connection keeps the key out unless includeSecret", async () => {
  const dir = tmp();
  fs.writeFileSync(path.join(dir, "roadie-api.json"), JSON.stringify({ port: 47630, token: "a".repeat(64) }));
  const f = fakeFetch(47630, { "GET /v1/tools/slskd/connection": [200, { url: "http://127.0.0.1:5030", apiKey: "k".repeat(48) }] });
  const api = new Api(dir, f);
  assert.equal((await callTool(api, "get_connection", { name: "slskd" })).apiKey, "<redacted>");
  assert.equal((await callTool(api, "get_connection", { name: "slskd", includeSecret: true })).apiKey, "k".repeat(48));
});

test("MCP framing: initialize, tools/list, unknown method, not-running error", async () => {
  const dir = tmp();
  const api = new Api(dir, fakeFetch(59998));
  const init = await handleMessage({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05" } }, { api });
  assert.equal(init.result.serverInfo.name, "roadie");
  assert.match(init.result.instructions, /Trust/);
  const list = await handleMessage({ jsonrpc: "2.0", id: 2, method: "tools/list" }, { api });
  assert.equal(list.result.tools.length, TOOLS.length);
  assert.ok(list.result.tools.every((t) => t.inputSchema.type === "object"));
  const unknown = await handleMessage({ jsonrpc: "2.0", id: 3, method: "nope" }, { api });
  assert.equal(unknown.error.code, -32601);
  assert.equal(await handleMessage({ jsonrpc: "2.0", method: "notifications/initialized" }, { api }), null);
  const down = await handleMessage({ jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "list_tools", arguments: {} } }, { api });
  assert.equal(down.result.isError, true);
  assert.match(down.result.content[0].text, /not running/);
});

test("launch command per platform", () => {
  assert.deepEqual(launchCommand("darwin"), ["open", ["-a", "Roadie"]]);
  assert.equal(launchCommand("linux")[0], "xdg-open");
});
