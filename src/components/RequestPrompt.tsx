import { useCallback } from "react";
import type { ConfigField, DryRun, Recipe, RoadieRequest, StoredRecipe, ToolRow } from "../types";
import { InstallProgressBar } from "./ToolRow";
import { ConfigForm } from "./ConfigForm";
import { InstallPlan } from "./InstallPlan";
import { installFields, isSettled, recipeChangeText, withRequestDecisions } from "../install";

interface Props {
  request: RoadieRequest;
  tool: ToolRow | undefined;
  /** The tool's recipe config fields (for the install decisions). */
  fields: ConfigField[];
  recipe: StoredRecipe | undefined;
  dryRun: (name: string) => Promise<DryRun>;
  /** Dry-run a recipe the request brought (not stored). */
  dryRunRecipe: (recipe: Recipe) => Promise<DryRun>;
  deciding: boolean;
  onDecide: (id: string, approve: boolean, answers?: Record<string, unknown>) => void;
  onReview: (name: string) => void;
  /** Open the review screen on the recipe this request brought. */
  onReviewBrought: (request: RoadieRequest) => void;
  /** The user has opened that review (approving needs it). */
  broughtReviewed: boolean;
}

/** One pending request from a local client or a deep link. Approving is the
 *  user's click that the API deliberately cannot make on its own. For an
 *  install, the recipe's `askOnInstall` fields are shown too: what the
 *  client decided is pre-filled, what it left open must be filled in. */
export function RequestPrompt({ request: r, tool, fields, recipe, dryRun, dryRunRecipe, deciding, onDecide, onReview, onReviewBrought, broughtReviewed }: Props) {
  // A recipe the app brought takes the stored one's place: it is what the
  // user reviews, what the decisions come from, and what approving trusts.
  const brought = r.recipe;
  // Keyed on the request, not the object: a refreshed list must not re-run the plan.
  const dryRunBrought = useCallback(() => (brought ? dryRunRecipe(brought) : Promise.reject(new Error("no recipe"))), [r.id, dryRunRecipe]);
  const name = brought?.displayName ?? tool?.displayName ?? r.tool;
  let title: string;
  let detail: string | null = null;
  switch (r.kind) {
    case "install":
      title = r.consumer ? `${r.requestedBy} asks to install ${name} and connect to it` : `${r.requestedBy} asks to install ${name}`;
      detail = r.consumer
        ? `${tool?.summary ?? ""} Approving also gives ${r.requestedBy} its own access key for ${name}, revocable any time from the tool's card.`.trim()
        : tool
          ? tool.summary
          : null;
      break;
    case "uninstall":
      title = `${r.requestedBy} asks to remove ${name}`;
      detail = r.keepData ? "Its settings will be kept." : "Its settings will be removed too. Your own folders are never deleted.";
      break;
    case "connect":
      title = `${r.requestedBy} wants to connect to ${name}`;
      detail = `It will receive its own access key for ${name}. You can revoke it any time from the tool's card.`;
      break;
    case "replaceRecipe":
      title = `${r.requestedBy} asks to change ${name}'s recipe`;
      detail = `${name} is installed. Approving trusts the new recipe, re-renders its files and updates it; a busy daemon is not restarted until it is idle.`;
      break;
  }
  if (brought && r.kind === "install") detail = brought.summary;
  // A draft blocks approval unless this request brings a recipe to trust.
  const untrusted = !brought && tool && !tool.trusted;
  const needsReview = !!brought && !broughtReviewed;
  const planRecipe = brought ?? recipe?.recipe;
  const planning = r.kind === "install" && !r.progress && !!planRecipe && (!!tool || !!brought);
  const formTool = tool ?? ({ name: r.tool, config: {} } as unknown as ToolRow);
  const decisions = planning ? installFields(brought?.config ?? fields, planRecipe) : [];
  const decidedByClient = decisions.filter((f) => isSettled(f, undefined, r));
  const open = decisions.filter((f) => !isSettled(f, tool, r));

  return (
    <div className={`prompt${planning ? " stacked" : ""}`}>
      <div className="prompt-body">
        <strong>{title}</strong>
        {detail ? <p>{detail}</p> : null}
        {brought && r.recipeChange ? (
          <p className="warn-text">
            {r.requestedBy} brings {recipeChangeText(r.recipeChange)}, by {brought.author || "an unnamed author"} (revision {brought.revision}). Approving trusts it.{" "}
            <button className="link" onClick={() => onReviewBrought(r)}>
              {broughtReviewed ? "Review it again" : "Review it"}
            </button>
            {needsReview ? " before approving." : null}
          </p>
        ) : null}
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
      {planning && planRecipe ? <InstallPlan recipe={planRecipe} dryRun={brought ? dryRunBrought : dryRun} /> : null}
      {planning ? (
        <ConfigForm
          tool={withRequestDecisions(formTool, r)}
          fields={decisions}
          saving={deciding || needsReview}
          submitLabel={brought ? "Trust recipe and install" : "Install"}
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
            if (untrusted || needsReview) return;
            onDecide(r.id, true, patch);
          }}
        />
      ) : (
        <div className="actions">
          <button className="ghost" disabled={deciding} onClick={() => onDecide(r.id, false)}>
            Decline
          </button>
          <button className="primary" disabled={deciding || !!untrusted || needsReview} onClick={() => onDecide(r.id, true)}>
            {r.kind === "install" ? (brought ? "Trust recipe and install" : "Install") : r.kind === "uninstall" ? "Remove" : r.kind === "replaceRecipe" ? "Trust recipe and update" : "Allow"}
          </button>
        </div>
      )}
    </div>
  );
}
