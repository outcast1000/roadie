import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ConfigField, DeferReason, InstallProgress, StoredRecipe, ToolRow as Row } from "../types";
import type { ToolsHook } from "../hooks/useTools";
import { ConfigForm } from "./ConfigForm";
import { platformChips, platformLabel } from "../search";
import { installFields } from "../install";
import { InstallPlan } from "./InstallPlan";
import type { DryRun } from "../types";

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function describeDefer(reason: DeferReason): string {
  switch (reason) {
    case "busy":
      return "waiting until it is idle";
    case "unreachable":
      return "waiting until it answers";
    case "restartNotAllowed":
      return "applied at the next start";
  }
}

export function stateLabel(t: Row): { text: string; tone: "muted" | "ok" | "warn" | "error" } {
  if (!t.trusted) return { text: "Draft recipe — review before installing", tone: "warn" };
  if (!t.supported) return { text: "Not available for this computer", tone: "muted" };
  if (!t.installed) return { text: "Not installed", tone: "muted" };
  if (t.kind === "cli") return { text: `Installed · ${t.version}`, tone: "ok" };
  if (t.conflict === "foreignInstanceOnPort") return { text: `Port in use by another ${t.displayName}`, tone: "error" };
  if (t.conflict === "startFailed") return { text: "Failed to start", tone: "error" };
  if (t.conflict) return { text: t.conflictDetail || "Blocked", tone: "error" };
  if (t.running && t.healthy) return { text: `Running · ${t.reportedVersion || t.version}`, tone: "ok" };
  if (t.running && t.starting) return { text: "Starting…", tone: "warn" };
  if (t.running) return { text: "Running · not answering", tone: "warn" };
  return { text: `Stopped · ${t.version}`, tone: "muted" };
}

/** The download/extract/verify bar shown on the card and in an approval
 *  prompt while an install runs. Indeterminate until the size is known. */
export function InstallProgressBar({ progress: p }: { progress: { phase: string; downloaded: number; total: number | null } }) {
  const known = p.phase === "downloading" && p.total ? p.total : null;
  const text =
    p.phase === "extracting"
      ? "Extracting…"
      : p.phase === "verifying"
        ? "Verifying the download…"
        : known
          ? `Downloading · ${Math.round((p.downloaded / known) * 100)}% of ${formatBytes(known)}`
          : `Downloading · ${formatBytes(p.downloaded)}`;
  return (
    <div className="progress" role="status">
      {known ? <progress value={p.downloaded} max={known} /> : <progress />}
      <span>{text}</span>
    </div>
  );
}

function progressText(p: InstallProgress | undefined): string | null {
  if (!p) return null;
  if (p.phase === "extracting") return "Extracting…";
  if (p.phase === "verifying") return "Verifying…";
  const pct = p.total ? Math.round((p.downloaded / p.total) * 100) : null;
  return pct === null ? `Downloading ${formatBytes(p.downloaded)}…` : `${pct}% · ${formatBytes(p.downloaded)}`;
}

interface Props {
  tool: Row;
  recipe: StoredRecipe | undefined;
  tools: ToolsHook;
  highlighted: boolean;
  onReview: (name: string) => void;
  onRevoke: (consumer: string, tool: string) => Promise<void>;
  dryRun: (name: string) => Promise<DryRun>;
}

export function ToolRow({ tool: t, recipe, tools, highlighted, onReview, onRevoke, dryRun }: Props) {
  const [showConfig, setShowConfig] = useState(false);
  const [showLogs, setShowLogs] = useState(false);
  const [logText, setLogText] = useState("");
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [askingInstall, setAskingInstall] = useState(false);
  const busy = tools.busy[t.name];
  const progress = progressText(tools.installing[t.name]);
  const label = stateLabel(t);
  const fields: ConfigField[] = recipe?.recipe.config ?? [];
  const decisions = installFields(fields, recipe?.recipe);
  const installProgress = tools.installing[t.name];
  const error = tools.errors[t.name];

  useEffect(() => {
    if (!showLogs) return;
    let alive = true;
    const load = () => tools.logs(t.name, 80).then((text) => alive && setLogText(text)).catch((e) => console.error("Failed to read logs:", e));
    load();
    const id = window.setInterval(load, 3000);
    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, [showLogs, t.name, tools]);

  const daemon = t.kind === "daemon";
  const canStart = t.trusted && t.installed && daemon && !t.running && t.conflict !== "foreignInstanceOnPort";

  return (
    <section className={`tool ${highlighted ? "highlighted" : ""}`} id={`tool-${t.name}`}>
      <header className="tool-head">
        <div className="tool-title">
          <h2>{t.displayName}</h2>
          <span className={`badge kind-${t.kind}`}>{t.kind === "daemon" ? "runs in background" : "command-line tool"}</span>
          {t.origin !== "builtin" ? <span className={`badge origin-${t.origin}`}>{t.origin === "draft" ? "draft" : "your recipe"}</span> : null}
        </div>
        <span className={`state tone-${label.tone}`}>{busy ? `${busy}…` : progress || label.text}</span>
      </header>
      <p className="summary">{t.summary}</p>
      <div className="meta">
        <span title="Who maintains this recipe">by {t.author}</span>
        <span title="The recipe's own edition, bumped by its author on every change">recipe rev. {t.revision}</span>
        <span className="platforms" title={t.platforms.map(platformLabel).join(", ")}>
          {platformChips(t.platforms).map((c) => (
            <span key={c} className="badge platform">
              {c}
            </span>
          ))}
        </span>
      </div>

      {error ? (
        <div className="callout error">
          <pre>{error}</pre>
          <button className="ghost small" onClick={() => tools.clearError(t.name)}>
            Dismiss
          </button>
        </div>
      ) : null}

      {!t.trusted ? (
        <div className="callout warn">
          This recipe was submitted{t.submittedBy ? ` by ${t.submittedBy}` : ""} and has not been reviewed. Read what it downloads and runs before trusting it.
          <button onClick={() => onReview(t.name)}>Review recipe</button>
        </div>
      ) : null}

      {t.conflict === "startFailed" && t.conflictDetail ? (
        <div className="callout error">
          <strong>It exited during startup.</strong>
          <pre>{t.conflictDetail}</pre>
        </div>
      ) : t.conflict && t.conflictDetail ? (
        <div className="callout error">{t.conflictDetail}</div>
      ) : null}
      {t.running && !t.healthy && !t.starting && t.healthDetail ? <div className="callout warn">Running but not answering: {t.healthDetail}</div> : null}
      {t.updateAvailable && t.latest ? (
        <div className="callout info">
          Update available: {t.version} → {t.latest}
          {t.updateStaged ? ` — downloaded, ${t.updateDeferredReason ? describeDefer(t.updateDeferredReason) : "ready"}` : ""}
        </div>
      ) : null}
      {installProgress ? <InstallProgressBar progress={installProgress} /> : null}
      {t.restartPending && t.running ? (
        <div className="callout info">Settings saved — restart pending{t.updateDeferredReason ? `, ${describeDefer(t.updateDeferredReason)}` : ""}.</div>
      ) : null}

      {t.installed ? (
        <dl className="facts">
          {t.url ? (
            <>
              <dt>Address</dt>
              <dd>
                <code>{t.url}</code>
              </dd>
            </>
          ) : null}
          {t.binPath ? (
            <>
              <dt>Command</dt>
              <dd>
                <code>{t.binPath}</code>
              </dd>
            </>
          ) : null}
          {Object.entries(t.details).map(([k, v]) => (
            <span key={k} className="fact-pair">
              <dt>{k.replace(/([A-Z])/g, " $1").toLowerCase()}</dt>
              <dd>{String(v)}</dd>
            </span>
          ))}
          {t.connectionPolicy !== "none" ? (
            <>
              <dt>Connected apps</dt>
              <dd>
                {t.approvedConsumers.length === 0
                  ? "none yet"
                  : t.approvedConsumers.map((c) => (
                      <span key={c} className="chip">
                        {c}
                        <button className="chip-x" title={`Revoke ${c}`} onClick={() => void onRevoke(c, t.name)}>
                          ×
                        </button>
                      </span>
                    ))}
              </dd>
            </>
          ) : null}
        </dl>
      ) : null}

      <div className="actions">
        {t.trusted && t.supported && !t.installed && !askingInstall ? (
          <button className="primary" disabled={!!busy} onClick={() => setAskingInstall(true)}>
            Install
          </button>
        ) : null}
        {t.installed && t.updateAvailable ? (
          <button disabled={!!busy} onClick={() => void tools.update(t.name)}>
            {t.updateStaged ? "Apply update" : "Update"}
          </button>
        ) : null}
        {canStart ? (
          <button className="primary" disabled={!!busy} onClick={() => void tools.start(t.name)}>
            {t.conflict ? "Try again" : "Start"}
          </button>
        ) : null}
        {t.installed && daemon && t.running ? (
          <button disabled={!!busy} onClick={() => void tools.stop(t.name)}>
            Stop
          </button>
        ) : null}
        {t.installed && fields.length > 0 ? (
          <button className="ghost" onClick={() => setShowConfig((s) => !s)}>
            {showConfig ? "Hide settings" : "Settings…"}
          </button>
        ) : null}
        {t.installed && t.url && t.running && t.healthy ? (
          <button className="ghost" onClick={() => void invoke("tool_open_url", { url: t.url })}>
            Open web UI
          </button>
        ) : null}
        {t.installed && daemon ? (
          <button className="ghost" onClick={() => setShowLogs((s) => !s)}>
            {showLogs ? "Hide log" : "Log"}
          </button>
        ) : null}
        {t.installed ? (
          <button className="ghost" disabled={!!busy} onClick={() => void tools.checkUpdates(t.name)} title="Look for a newer release now">
            Check for updates
          </button>
        ) : null}
        {t.homepage ? (
          <button className="ghost" onClick={() => void invoke("tool_open_url", { url: t.homepage })}>
            Website
          </button>
        ) : null}
        {t.installed && !confirmRemove ? (
          <button className="ghost danger" disabled={!!busy} onClick={() => setConfirmRemove(true)}>
            Remove
          </button>
        ) : null}
        {t.installed && daemon ? (
          <label className="toggle">
            <input type="checkbox" checked={t.autostart} disabled={!!busy} onChange={(e) => void tools.setAutostart(t.name, e.target.checked)} />
            Start at login
          </label>
        ) : null}
      </div>

      {askingInstall && recipe ? (
        <div className="install-prompt">
          <InstallPlan recipe={recipe.recipe} dryRun={dryRun} />
          <ConfigForm
            tool={t}
            fields={decisions}
            saving={!!busy}
            submitLabel="Install"
            busyLabel="Installing…"
            sendAll
            intro={
              decisions.length > 0
                ? `A few things to decide before ${t.displayName} is installed. You can change them later under Settings.`
                : `Nothing to decide for ${t.displayName}.`
            }
            onCancel={() => setAskingInstall(false)}
            onSave={async (patch) => {
              setAskingInstall(false);
              await tools.install(t.name, patch);
            }}
          />
        </div>
      ) : null}

      {confirmRemove ? (
        <div className="callout">
          Remove {t.displayName}? Your downloads and other folders it wrote to are never deleted.
          <div className="actions">
            <button className="ghost" onClick={() => setConfirmRemove(false)}>
              Cancel
            </button>
            <button
              onClick={() => {
                setConfirmRemove(false);
                void tools.uninstall(t.name, true);
              }}
            >
              Remove, keep settings
            </button>
            <button
              className="danger"
              onClick={() => {
                setConfirmRemove(false);
                void tools.uninstall(t.name, false);
              }}
            >
              Remove everything
            </button>
          </div>
        </div>
      ) : null}

      {showConfig ? (
        <ConfigForm
          tool={t}
          fields={fields}
          saving={!!busy}
          onCancel={() => setShowConfig(false)}
          onSave={async (patch) => {
            await tools.configure(t.name, patch);
            setShowConfig(false);
          }}
        />
      ) : null}

      {showLogs ? (
        <div className="log">
          <div className="log-head">
            <span title={t.logsDir}>{t.logsDir}</span>
            <button className="ghost small" onClick={() => void tools.openLogs(t.name)}>
              Show folder
            </button>
          </div>
          <pre>{logText || "(empty)"}</pre>
        </div>
      ) : null}

      {t.notes ? <p className="notes">{t.notes}</p> : null}
    </section>
  );
}
