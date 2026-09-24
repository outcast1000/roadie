// Which config fields an install has to settle first, and which of them a
// request already settled. Pure, so vitest covers it.

import type { ConfigField, Recipe, RecipeChange, RoadieRequest, ToolRow } from "./types";

/** What an app's own recipe does to Roadie's, in the prompt's words. */
export function recipeChangeText(change: RecipeChange): string {
  switch (change) {
    case "new":
      return "a recipe for a tool Roadie does not know yet";
    case "replacesBuiltin":
      return "its own recipe, replacing Roadie's built-in one";
    case "changesTrusted":
      return "a changed recipe, replacing the one you trusted";
    case "replacesDraft":
      return "its own recipe, replacing an unreviewed draft";
  }
}

/** The engine's own install choices, rendered like bool config fields so
 *  the same form asks them. Keys are reserved (`startNow`, `autostart`). */
export function engineChoices(recipe: Pick<Recipe, "kind" | "startAfterInstall" | "autostart"> | undefined): ConfigField[] {
  if (!recipe || recipe.kind !== "daemon") return [];
  const out: ConfigField[] = [];
  if (recipe.startAfterInstall?.askOnInstall) {
    out.push({ key: "startNow", label: "Start now, right after installing", kind: "bool", default: recipe.startAfterInstall.default, askOnInstall: true });
  }
  if (recipe.autostart?.askOnInstall) {
    out.push({
      key: "autostart",
      label: "Start at login",
      kind: "bool",
      default: recipe.autostart.default,
      askOnInstall: true,
      help: "Roadie's background service starts it when you log in (needs \"Run in the background\" in Settings). Nothing system-wide; switch it off any time from the card.",
    });
  }
  return out;
}

/** The recipe's `askOnInstall` fields plus every `required` one, followed
 *  by the engine choices the recipe offers. */
export function installFields(fields: ConfigField[], recipe?: Pick<Recipe, "kind" | "startAfterInstall" | "autostart">): ConfigField[] {
  return [...fields.filter((f) => f.askOnInstall || f.required), ...engineChoices(recipe)];
}

/** True when a value for `f` already exists — in the tool's stored config
 *  (`has_<key>` for passwords) or in what an API client sent along. */
export function isSettled(f: ConfigField, tool: Pick<ToolRow, "config"> | undefined, request?: Pick<RoadieRequest, "config" | "secretKeys">): boolean {
  if (request) {
    if (f.secret && request.secretKeys?.includes(f.key)) return true;
    const v = request.config?.[f.key];
    if (v !== undefined && v !== null && v !== "") return true;
  }
  // A yes/no with a recipe default is settled by that default.
  if (f.kind === "bool" && f.default !== undefined) return true;
  if (!tool) return false;
  if (f.secret) return tool.config[`has_${f.key}`] === true;
  const v = tool.config[f.key];
  return v !== undefined && v !== null && v !== "";
}

/** A tool row whose config reflects an API client's decisions, so the
 *  approval prompt's form starts from them rather than from the defaults. */
export function withRequestDecisions(tool: ToolRow, request: Pick<RoadieRequest, "config" | "secretKeys">): ToolRow {
  const config = { ...tool.config, ...(request.config ?? {}) };
  for (const k of request.secretKeys ?? []) config[`has_${k}`] = true;
  return { ...tool, config };
}
