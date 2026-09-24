import { useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type { ConfigField, ToolRow } from "../types";

const TCC_FOLDERS = ["/Downloads", "/Documents", "/Desktop"];

function isTccPath(p: string): boolean {
  return navigator.platform.toLowerCase().includes("mac") && TCC_FOLDERS.some((f) => p.includes(f + "/") || p.endsWith(f));
}

interface Props {
  tool: ToolRow;
  fields: ConfigField[];
  saving: boolean;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  onCancel: () => void;
  /** "Install" on the install prompt; defaults to "Save". */
  submitLabel?: string;
  busyLabel?: string;
  /** Shown above the fields (why they are being asked). */
  intro?: React.ReactNode;
  /** Send every field's value, not just the changed ones (install prompt). */
  sendAll?: boolean;
}

/** Renders a recipe's `config` fields. Secret fields show only whether a
 *  value is set; leaving one blank keeps it, typing replaces it, and the
 *  explicit Clear button empties it. */
export function ConfigForm({ tool, fields, saving, onSave, onCancel, submitLabel, busyLabel, intro, sendAll }: Props) {
  const initial: Record<string, unknown> = {};
  for (const f of fields) {
    if (f.secret) continue;
    initial[f.key] = tool.config[f.key] ?? (f.kind === "bool" ? (typeof f.default === "boolean" ? f.default : false) : "");
  }
  const [values, setValues] = useState<Record<string, unknown>>(initial);
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [cleared, setCleared] = useState<Record<string, boolean>>({});
  const [problem, setProblem] = useState<string | null>(null);

  const set = (k: string, v: unknown) => setValues((prev) => ({ ...prev, [k]: v }));

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setProblem(null);
    const patch: Record<string, unknown> = {};
    for (const f of fields) {
      if (f.secret) {
        if (cleared[f.key]) patch[f.key] = "";
        else if (secrets[f.key]) patch[f.key] = secrets[f.key];
        continue;
      }
      const v = values[f.key];
      if (f.kind === "port") {
        const n = Number(v);
        if (v !== "" && (!Number.isInteger(n) || n < 1 || n > 65535)) {
          setProblem(`${f.label} must be a port between 1 and 65535`);
          return;
        }
        if (v !== "") patch[f.key] = n;
        continue;
      }
      if (f.required && (v === "" || v === undefined)) {
        setProblem(`${f.label} is required`);
        return;
      }
      if (sendAll ? v !== "" && v !== undefined : v !== tool.config[f.key]) patch[f.key] = v;
    }
    await onSave(patch);
  };

  return (
    <form className="config-form" onSubmit={submit}>
      {intro ? <p className="form-intro">{intro}</p> : null}
      {fields.map((f) => (
        <label key={f.key} className={`field field-${f.kind}`}>
          <span className="field-label">
            {f.label}
            {f.required ? " *" : ""}
          </span>
          {f.kind === "bool" ? (
            <input type="checkbox" checked={Boolean(values[f.key])} onChange={(e) => set(f.key, e.target.checked)} />
          ) : f.kind === "password" ? (
            <span className="field-row">
              <input
                type="password"
                placeholder={tool.config[`has_${f.key}`] && !cleared[f.key] ? "•••••••• (unchanged)" : ""}
                value={secrets[f.key] ?? ""}
                onChange={(e) => {
                  setSecrets((s) => ({ ...s, [f.key]: e.target.value }));
                  setCleared((c) => ({ ...c, [f.key]: false }));
                }}
              />
              {tool.config[`has_${f.key}`] && !cleared[f.key] ? (
                <button type="button" className="ghost" onClick={() => setCleared((c) => ({ ...c, [f.key]: true }))}>
                  Clear
                </button>
              ) : null}
            </span>
          ) : f.kind === "path" ? (
            <span className="field-row">
              <input type="text" value={String(values[f.key] ?? "")} onChange={(e) => set(f.key, e.target.value)} />
              <button
                type="button"
                className="ghost"
                onClick={async () => {
                  try {
                    const picked = await openDialog({ directory: true, multiple: false, defaultPath: String(values[f.key] || "") || undefined });
                    if (typeof picked === "string") set(f.key, picked);
                  } catch (e) {
                    console.error("Folder picker failed:", e);
                  }
                }}
              >
                Choose…
              </button>
            </span>
          ) : (
            <input type={f.kind === "port" ? "number" : "text"} value={String(values[f.key] ?? "")} onChange={(e) => set(f.key, e.target.value)} />
          )}
          {f.help ? <span className="field-help">{f.help}</span> : null}
          {f.kind === "path" && f.tccSensitive && isTccPath(String(values[f.key] ?? "")) ? (
            <span className="field-warn">
              macOS may refuse a background program access to this folder. A folder under ~/Music or your home folder avoids the prompt.
            </span>
          ) : null}
        </label>
      ))}
      {problem ? <p className="error">{problem}</p> : null}
      <div className="actions">
        <button type="button" className="ghost" onClick={onCancel} disabled={saving}>
          Cancel
        </button>
        <button type="submit" className={submitLabel ? "primary" : ""} disabled={saving}>
          {saving ? (busyLabel ?? "Saving…") : (submitLabel ?? "Save")}
        </button>
      </div>
    </form>
  );
}
