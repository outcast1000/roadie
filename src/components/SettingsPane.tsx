import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import type { AppInfo, ConsumerPublic, McpSetupInfo, Settings } from "../types";

interface Props {
  consumers: ConsumerPublic[];
  onRevoke: (consumer: string, tool?: string) => Promise<void>;
}

async function copy(text: string, setNote: (s: string) => void) {
  try {
    await navigator.clipboard.writeText(text);
    setNote("Copied.");
  } catch (e) {
    console.error("Clipboard write failed:", e);
    setNote("Could not copy — select the text and copy it by hand.");
  }
  window.setTimeout(() => setNote(""), 2500);
}

export function SettingsPane({ consumers, onRevoke }: Props) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [mcp, setMcp] = useState<McpSetupInfo | null>(null);
  const [note, setNote] = useState("");
  const [updateState, setUpdateState] = useState<string>("");

  useEffect(() => {
    invoke<Settings>("settings_get").then(setSettings).catch((e) => console.error("Failed to load settings:", e));
    invoke<AppInfo>("app_info").then(setInfo).catch((e) => console.error("Failed to load app info:", e));
    invoke<McpSetupInfo>("mcp_setup_info").then(setMcp).catch((e) => console.error("Failed to load MCP info:", e));
  }, []);

  const save = async (next: Settings) => {
    try {
      setSettings(await invoke<Settings>("settings_set", { settings: next }));
    } catch (e) {
      console.error("Failed to save settings:", e);
    }
  };

  const checkAppUpdate = async () => {
    setUpdateState("Checking…");
    try {
      const u = await check();
      if (!u) {
        setUpdateState("Roadie is up to date.");
        return;
      }
      setUpdateState(`Downloading ${u.version}…`);
      await u.downloadAndInstall();
      setUpdateState("Installed — relaunching.");
      await relaunch();
    } catch (e) {
      console.error("Update check failed:", e);
      setUpdateState(`Update check failed: ${String(e)}`);
    }
  };

  const mcpConfig =
    mcp?.scriptPath && mcp.nodePath
      ? JSON.stringify({ roadie: { command: mcp.nodePath, args: [mcp.scriptPath] } }, null, 2)
      : null;
  const mcpCommand = mcp?.scriptPath && mcp.nodePath ? `claude mcp add roadie -- "${mcp.nodePath}" "${mcp.scriptPath}"` : null;

  return (
    <div className="settings">
      <section className="card">
        <h2>Background service</h2>
        <label className="toggle">
          <input type="checkbox" checked={settings?.runInBackground ?? true} disabled={!settings} onChange={(e) => settings && void save({ ...settings, runInBackground: e.target.checked })} />
          Run in the background
        </label>
        <p className="muted">
          On: Roadie's service starts when you log in and keeps running when this window is closed, so other apps and assistants can reach the local API, tools marked
          "start at login" are started, and daily updates happen. Off: Roadie behaves like a plain app; the service stops a few seconds after you close this window and
          nothing starts at login. Tools you started keep running either way.
        </p>
        {info?.service ? (
          <p className="muted">
            Service {info.service.version} · pid {info.service.pid} · {info.service.loginItem ? "login item registered" : "no login item"} ·{" "}
            {info.service.ownerChannel ? "owner channel connected" : "owner channel not connected — approvals unavailable"}
          </p>
        ) : (
          <p className="warn-text">Not connected to the service.</p>
        )}
      </section>

      <section className="card">
        <h2>Updates</h2>
        <label className="toggle">
          <input type="checkbox" checked={settings?.autoUpdateTools ?? true} disabled={!settings} onChange={(e) => settings && void save({ ...settings, autoUpdateTools: e.target.checked })} />
          Keep installed tools up to date automatically
        </label>
        <p className="muted">Checked once a day. A running tool is only restarted for an update when it is idle; otherwise the update waits.</p>
        <div className="actions">
          <button onClick={() => void checkAppUpdate()}>Check for Roadie updates</button>
          <span className="muted">{updateState || (info ? `Roadie ${info.version}` : "")}</span>
        </div>
      </section>

      <section className="card">
        <h2>Connected apps</h2>
        <p className="muted">Apps you have allowed to read a tool's connection details. Revoking removes their key at the tool's next restart.</p>
        {consumers.length === 0 ? <p className="muted">None registered.</p> : null}
        <ul className="consumers">
          {consumers.map((c) => (
            <li key={c.id}>
              <strong>{c.displayName || c.id}</strong> <code>{c.id}</code>
              {c.tools.length === 0 ? (
                <span className="muted"> — no access granted</span>
              ) : (
                c.tools.map((t) => (
                  <span key={t} className="chip">
                    {t}
                    <button className="chip-x" title={`Revoke ${t}`} onClick={() => void onRevoke(c.id, t)}>
                      ×
                    </button>
                  </span>
                ))
              )}
              {!c.builtin ? (
                <button className="ghost small danger" onClick={() => void onRevoke(c.id)}>
                  Forget
                </button>
              ) : null}
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>Local API</h2>
        {info ? (
          <p className="muted">
            The service listens on <code>http://127.0.0.1:{info.apiPort ?? "…"}</code>. Read-only status needs no token; control and recipe authoring need the bearer
            token from <code>{info.dataDir}/roadie-api.json</code>. Installing or removing a tool through the API always asks you here, and only this window can
            approve: it proves itself to the service over a local channel that other programs cannot use.
          </p>
        ) : null}
      </section>

      <section className="card">
        <h2>AI assistants (MCP)</h2>
        <p className="muted">
          Roadie ships an MCP server so Claude Desktop, Claude Code or Cursor can check tools, start and stop them, and write new recipes for you to review. Nothing installs without your
          approval here.
        </p>
        {mcp?.problem ? <p className="warn-text">{mcp.problem}</p> : null}
        {mcpConfig ? (
          <>
            <pre className="mono">{mcpConfig}</pre>
            <div className="actions">
              <button onClick={() => void copy(mcpConfig, setNote)}>Copy config</button>
              {mcpCommand ? <button onClick={() => void copy(mcpCommand, setNote)}>Copy Claude Code command</button> : null}
              <span className="muted">{note}</span>
            </div>
            <p className="muted">
              Node {mcp?.nodeVersion} at <code>{mcp?.nodePath}</code>
            </p>
          </>
        ) : null}
      </section>

      {info ? (
        <section className="card">
          <h2>About</h2>
          <p className="muted">
            Roadie {info.version} · {info.platform} · data in <code>{info.dataDir}</code>
          </p>
        </section>
      ) : null}
    </div>
  );
}
