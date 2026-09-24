import { useEffect, useState } from "react";
import type { DryRun, StoredRecipe } from "../types";
import { platformLabel } from "../search";

interface Props {
  stored: StoredRecipe;
  dryRun: (name: string) => Promise<DryRun>;
  onTrust: (name: string) => Promise<void>;
  onDelete: (name: string) => Promise<void>;
  onClose: () => void;
}

/** What a recipe would do, laid out for a human: where it downloads from,
 *  what it runs, which files it writes, what it exposes. Trusting is the
 *  user's click that makes it installable. */
export function RecipeReview({ stored, dryRun, onTrust, onDelete, onClose }: Props) {
  const r = stored.recipe;
  const [dry, setDry] = useState<DryRun | null>(null);
  const [dryError, setDryError] = useState<string | null>(null);
  const [showJson, setShowJson] = useState(false);
  const [working, setWorking] = useState(false);

  useEffect(() => {
    let alive = true;
    setDry(null);
    setDryError(null);
    dryRun(r.name)
      .then((d) => alive && setDry(d))
      .catch((e) => {
        console.error("Dry run failed:", e);
        if (alive) setDryError(String(e));
      });
    return () => {
      alive = false;
    };
  }, [r.name, dryRun]);

  const source = r.source as Record<string, unknown>;
  const urls: string[] = [];
  if (source.kind === "githubRelease") urls.push(`https://github.com/${String(source.repo)}/releases`);
  if (source.kind === "httpRedirect") for (const u of Object.values((source.latestUrl as Record<string, string>) || {})) urls.push(u);
  if (dry?.resolved) urls.push(dry.resolved.downloadUrl);

  return (
    <div className="review">
      <header className="review-head">
        <div>
          <h2>{r.displayName}</h2>
          <span className={`badge origin-${stored.origin}`}>{stored.origin === "draft" ? `draft${stored.submittedBy ? ` from ${stored.submittedBy}` : ""}` : stored.origin}</span>
          <span className={`badge kind-${r.kind}`}>{r.kind}</span>
        </div>
        <button className="ghost" onClick={onClose}>
          Close
        </button>
      </header>
      <p className="summary">{r.summary}</p>
      {r.notes ? <p className="notes">{r.notes}</p> : null}

      <dl className="facts">
        <dt>Author</dt>
        <dd>{r.author}</dd>
        <dt>Recipe revision</dt>
        <dd>{r.revision}</dd>
        <dt>Targets</dt>
        <dd>
          {r.platforms.map((p) => (
            <span key={p} className="badge platform">
              {platformLabel(p)}
            </span>
          ))}
        </dd>
        {r.license ? (
          <>
            <dt>License</dt>
            <dd>{r.license}</dd>
          </>
        ) : null}
      </dl>

      <h3>Downloads from</h3>
      <ul className="mono">
        {urls.map((u) => (
          <li key={u}>{u}</li>
        ))}
        {dry?.resolved ? (
          <li className="muted">
            latest: {dry.resolved.version} · {dry.resolved.checksumsUrl ? "checksum verified" : "no upstream checksum — verified by running --version"} ·{" "}
            {dry.assetReachable === null ? "" : dry.assetReachable ? "URL answers" : "URL did not answer"}
          </li>
        ) : null}
        {dry?.resolveError ? <li className="warn-text">Could not resolve the latest release: {dry.resolveError}</li> : null}
        {dry && !dry.supported ? <li className="muted">Not available for this computer ({dry.platform}).</li> : null}
      </ul>
      <p className="muted">
        Verification: runs <code>{(r.layout?.binaries?.[0] ?? r.name) + " " + ((r as unknown as { version?: { args?: string[] } }).version?.args ?? []).join(" ")}</code> after extraction and requires the version to match.
      </p>

      {r.kind === "daemon" && r.run ? (
        <>
          <h3>Runs</h3>
          <pre className="mono">{[r.layout?.binaries?.[0] ?? r.name, ...(dry?.runArgs ?? r.run.args)].join(" ")}</pre>
          {r.run.env && Object.keys(r.run.env).length ? (
            <pre className="mono">
              {Object.entries(r.run.env)
                .map(([k, v]) => `${k}=${v}`)
                .join("\n")}
            </pre>
          ) : null}
          {r.ports ? (
            <p className="muted">
              Ports: {Object.entries(r.ports).map(([k, p]) => `${k} ${p.default}`).join(", ")}. {r.connection ? `Exposes ${r.connection.url} (${r.connection.policy}).` : "Exposes no connection."}
            </p>
          ) : null}
        </>
      ) : (
        <p className="muted">A command-line tool: installed under Roadie's bin folder, never started by Roadie.</p>
      )}

      {(r.createDirs?.length || dry?.createDirs.length) ? (
        <>
          <h3>Creates folders</h3>
          <ul className="mono">
            {(dry?.createDirs ?? r.createDirs ?? []).map((d) => (
              <li key={d}>{d}</li>
            ))}
          </ul>
        </>
      ) : null}

      {r.files && r.files.length ? (
        <>
          <h3>Writes files</h3>
          {dryError ? <p className="warn-text">{dryError}</p> : null}
          {(dry?.files ?? []).map((f) => (
            <details key={f.path} open>
              <summary className="mono">
                {f.path} {f.secret ? "(private, 0600)" : ""}
              </summary>
              <pre className="mono">{f.contents}</pre>
            </details>
          ))}
          {!dry && !dryError ? <p className="muted">Rendering…</p> : null}
        </>
      ) : null}

      <h3>Settings it asks you for</h3>
      <ul>
        {r.config.length === 0 ? <li className="muted">none</li> : null}
        {r.config.map((f) => (
          <li key={f.key}>
            {f.label} <span className="muted">({f.kind}{f.secret ? ", secret" : ""}{f.required ? ", required" : ""})</span>
          </li>
        ))}
      </ul>

      <button className="ghost small" onClick={() => setShowJson((s) => !s)}>
        {showJson ? "Hide" : "Show"} recipe JSON
      </button>
      {showJson ? <pre className="mono json">{JSON.stringify(r, null, 2)}</pre> : null}

      <div className="actions review-actions">
        {stored.origin !== "builtin" ? (
          <button
            className="ghost danger"
            disabled={working}
            onClick={async () => {
              setWorking(true);
              await onDelete(r.name);
              setWorking(false);
              onClose();
            }}
          >
            Delete recipe
          </button>
        ) : null}
        {stored.origin === "draft" ? (
          <button
            className="primary"
            disabled={working}
            onClick={async () => {
              setWorking(true);
              await onTrust(r.name);
              setWorking(false);
              onClose();
            }}
          >
            Trust this recipe
          </button>
        ) : null}
      </div>
    </div>
  );
}
