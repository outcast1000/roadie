import { useEffect, useState } from "react";
import type { DryRun, Recipe } from "../types";

interface Props {
  recipe: Recipe;
  dryRun: (name: string) => Promise<DryRun>;
}

/** What installing this tool will change on this computer, shown before
 *  the user confirms: the download, where it lands, the files and folders
 *  written, the ports and process a daemon will use. Built from a dry run,
 *  so it is the engine's own answer, not a paraphrase of the recipe. */
export function InstallPlan({ recipe: r, dryRun }: Props) {
  const [dry, setDry] = useState<DryRun | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setDry(null);
    setError(null);
    dryRun(r.name)
      .then((d) => alive && setDry(d))
      .catch((e) => {
        console.error("Failed to plan install:", e);
        if (alive) setError(String(e));
      });
    return () => {
      alive = false;
    };
  }, [r.name, dryRun]);

  const binary = r.layout?.binaries?.[0] ?? r.name;

  return (
    <div className="plan">
      <h4>What will change on this computer</h4>
      {error ? <p className="warn-text">Could not work out the plan: {error}</p> : null}
      {!dry && !error ? <p className="muted">Checking the latest release…</p> : null}
      {dry ? (
        <dl className="facts">
          <dt>Downloads</dt>
          <dd>
            {dry.resolved ? (
              <span>
                <code>{dry.resolved.downloadUrl}</code>
                <span className="muted">
                  {" "}
                  · version {dry.resolved.version} ·{" "}
                  {dry.resolved.checksumsUrl ? "checksum verified" : "no upstream checksum; verified by running its version flag"}
                  {dry.assetReachable === false ? " · the URL did not answer" : ""}
                </span>
              </span>
            ) : dry.resolveError ? (
              <span className="warn-text">Could not resolve the latest release: {dry.resolveError}</span>
            ) : (
              <span className="muted">Not available for this computer ({dry.platform}).</span>
            )}
          </dd>

          <dt>Unpacks to</dt>
          <dd>
            <code>{dry.installDir}</code>
          </dd>

          {dry.binPath ? (
            <>
              <dt>Command</dt>
              <dd>
                <code>{dry.binPath}</code> <span className="muted">a link to the current version; nothing else on your PATH changes</span>
              </dd>
            </>
          ) : null}

          {dry.files.length > 0 ? (
            <>
              <dt>Writes</dt>
              <dd className="stack">
                {dry.files.map((f) => (
                  <details key={f.path}>
                    <summary>
                      <code>{f.path}</code> {f.secret ? <span className="muted">private, 0600</span> : null}
                    </summary>
                    <pre className="mono">{f.contents}</pre>
                  </details>
                ))}
                <span className="muted">
                  plus its state in <code>{dry.dataDir}</code> and logs in <code>{dry.logsDir}</code>
                </span>
              </dd>
            </>
          ) : (
            <>
              <dt>Writes</dt>
              <dd>
                <span className="muted">
                  only its state in <code>{dry.dataDir}</code>
                </span>
              </dd>
            </>
          )}

          {dry.createDirs.length > 0 ? (
            <>
              <dt>Creates folders</dt>
              <dd className="stack">
                {dry.createDirs.map((d) => (
                  <code key={d}>{d}</code>
                ))}
              </dd>
            </>
          ) : null}

          {r.kind === "daemon" ? (
            <>
              <dt>Runs</dt>
              <dd className="stack">
                <code>{[binary, ...dry.runArgs].join(" ")}</code>
                <span className="muted">
                  Starts right after installing and at login only if you say so below; otherwise from Start on its card. Stop from the
                  card too. It keeps running after Roadie quits.
                  {Object.keys(dry.ports).length > 0
                    ? ` Listens on 127.0.0.1 only, port${Object.keys(dry.ports).length > 1 ? "s" : ""} ${Object.entries(dry.ports)
                        .map(([k, p]) => `${p} (${k})`)
                        .join(", ")}.`
                    : ""}
                  {dry.connectionUrl ? ` Apps you approve reach it at ${dry.connectionUrl}.` : ""}
                </span>
              </dd>
            </>
          ) : (
            <>
              <dt>Runs</dt>
              <dd>
                <span className="muted">Nothing. A command-line tool is never started by Roadie.</span>
              </dd>
            </>
          )}
          <dt>Never</dt>
          <dd>
            <span className="muted">
              No system-wide install, nothing outside Roadie's data folder and the folders listed here.
              {r.kind === "daemon" ? " No login item of its own: if you tick \"start at login\" below, Roadie's background service starts it." : " No login item."}
            </span>
          </dd>
        </dl>
      ) : null}
    </div>
  );
}
