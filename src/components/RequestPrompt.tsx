import type { ConfigField, DryRun, RoadieRequest, StoredRecipe, ToolRow } from "../types";
import { InstallProgressBar } from "./ToolRow";
import { ConfigForm } from "./ConfigForm";
import { InstallPlan } from "./InstallPlan";
import { installFields, isSettled, withRequestDecisions } from "../install";

interface Props {
  request: RoadieRequest;
  tool: ToolRow | undefined;
  /** The tool's recipe config fields (for the install decisions). */
  fields: ConfigField[];
  recipe: StoredRecipe | undefined;
  dryRun: (name: string) => Promise<DryRun>;
  deciding: boolean;
  onDecide: (id: string, approve: boolean, answers?: Record<string, unknown>) => void;
  onReview: (name: string) => void;
}

/** One pending request from a local client or a deep link. Approving is the
 *  user's click that the API deliberately cannot make on its own. For an
 *  install, the recipe's `askOnInstall` fields are shown too: what the
 *  client decided is pre-filled, what it left open must be filled in. */
export function RequestPrompt({ request: r, tool, fields, recipe, dryRun, deciding, onDecide, onReview }: Props) {
  const name = tool?.displayName ?? r.tool;
  let title: string;
  let detail: string | null = null;
  switch (r.kind) {
    case "install":
      title = `${r.requestedBy} asks to install ${name}`;
      detail = tool ? tool.summary : null;
      break;
    case "uninstall":
      title = `${r.requestedBy} asks to remove ${name}`;
      detail = r.keepData ? "Its settings will be kept." : "Its settings will be removed too. Your own folders are never deleted.";
      break;
    case "connect":
      title = `${r.requestedBy} wants to connect to ${name}`;
      detail = `It will receive its own access key for ${name}. You can revoke it any time from the tool's card.`;
      break;
  }
  const untrusted = tool && !tool.trusted;
  const planning = r.kind === "install" && tool && !r.progress && !!recipe;
  const decisions = planning ? installFields(fields, recipe?.recipe) : [];
  const decidedByClient = decisions.filter((f) => isSettled(f, undefined, r));
  const open = decisions.filter((f) => !isSettled(f, tool, r));

  return (
    <div className={`prompt${planning ? " stacked" : ""}`}>
      <div className="prompt-body">
        <strong>{title}</strong>
        {detail ? <p>{detail}</p> : null}
        {untrusted ? (
          <p className="warn-text">
            This tool's recipe is an unreviewed draft. <button className="link" onClick={() => onReview(r.tool)}>Review it</button> before installing.
          </p>
        ) : null}
        {decidedByClient.length > 0 ? (
          <p className="muted">
            {r.requestedBy} decided:{" "}
            {decidedByClient.map((f, i) => (
              <span key={f.key}>
                {i > 0 ? " · " : ""}
                {f.label}: <strong>{f.secret ? "provided" : typeof r.config?.[f.key] === "boolean" ? (r.config[f.key] ? "yes" : "no") : String(r.config?.[f.key])}</strong>
              </span>
            ))}
          </p>
        ) : null}
        {r.progress ? <InstallProgressBar progress={r.progress} /> : null}
      </div>
      {planning && recipe ? <InstallPlan recipe={recipe.recipe} dryRun={dryRun} /> : null}
      {planning && tool ? (
        <ConfigForm
          tool={withRequestDecisions(tool, r)}
          fields={decisions}
          saving={deciding}
          submitLabel="Install"
          busyLabel="Installing…"
          sendAll
          intro={
            open.length > 0
              ? `Before installing, please decide: ${open.map((f) => f.label).join(", ")}. You can change any value the client chose.`
              : decisions.length > 0
                ? "Check the decisions below, change anything you like, then install."
                : "Nothing to decide."
          }
          onCancel={() => onDecide(r.id, false)}
          onSave={async (patch) => {
            if (untrusted) return;
            onDecide(r.id, true, patch);
          }}
        />
      ) : (
        <div className="actions">
          <button className="ghost" disabled={deciding} onClick={() => onDecide(r.id, false)}>
            Decline
          </button>
          <button className="primary" disabled={deciding || !!untrusted} onClick={() => onDecide(r.id, true)}>
            {r.kind === "install" ? "Install" : r.kind === "uninstall" ? "Remove" : "Allow"}
          </button>
        </div>
      )}
    </div>
  );
}
