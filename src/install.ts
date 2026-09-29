// Which config fields an install has to settle first, and which of them a
// request already settled. Pure, so vitest covers it.

import type { ConfigField, Recipe, RecipeChange, RecipeSource, RoadieRequest, ToolRow } from "./types";

/** The sentence a prompt says about a request's recipe: who it comes from and what it replaces. */
export function recipeOriginText(requestedBy: string, change: RecipeChange, source?: RecipeSource): string {
  if (source === "catalog") {
    return change === "changesTrusted"
      ? "The Roadie recipe catalog has a new revision of this recipe, replacing the one you trusted"
      : "The recipe comes from the Roadie recipe catalog and has not been reviewed on this computer";
  }
  return `${requestedBy} brings ${recipeChangeText(change)}`;
}

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

/** The engine's own install choices, rendered like config fields so the
 *  same form asks them: where it installs (`installDir`, default shown),
 *  the ports and secrets the recipe offers (`ports.<name>`,
 *  `secrets.<key>`), and for a daemon `startNow`/`autostart`. */
export function engineChoices(
  recipe: Pick<Recipe, "kind" | "startAfterInstall" | "autostart" | "ports" | "secrets"> | undefined,
  versionsDir?: string,
): ConfigField[] {
  if (!recipe) return [];
  const out: ConfigField[] = [];
  out.push({
    key: "installDir",
    label: "Install folder",
    kind: "path",
    default: versionsDir || null,
    askOnInstall: true,
    help: "Where Roadie unpacks it. A new or empty folder; Roadie removes it when you uninstall.",
  });
  for (const [name, p] of Object.entries(recipe.ports ?? {})) {
    if (p.askOnInstall) out.push({ key: `ports.${name}`, label: p.label || name, kind: "port", default: p.default, askOnInstall: true });
  }
  for (const s of recipe.secrets ?? []) {
    const required = !s.generate;
    if (s.askOnInstall || required) {
      out.push({
        key: `secrets.${s.key}`,
        label: s.label || s.key,
        // Plain text on purpose: the user may see and copy the key.
        kind: "text",
        required,
        askOnInstall: true,
        help: required
          ? `Required: Roadie cannot generate it. At least ${s.minLen ?? 16} characters.`
          : `Leave blank and Roadie generates one. At least ${s.minLen ?? 16} characters. You can see it on the card once installed.`,
      });
    }
  }
  if (recipe.kind !== "daemon") return out;
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

/** Every field a recipe offers: its `config` fields, then its
 *  `configuration` entries as fields keyed by their entry path — the same
 *  list the engine builds (`Recipe::fields`). */
export function recipeFields(recipe?: Pick<Recipe, "config" | "configuration">): ConfigField[] {
  if (!recipe) return [];
  const entries = (recipe.configuration ?? []).map((e) => ({
    key: e.entry,
    label: e.label,
    help: e.help,
    kind: e.kind,
    required: e.required,
    default: e.default,
    askOnInstall: e.askOnInstall,
  }));
  return [...recipe.config, ...entries];
}

/** The recipe's `askOnInstall` fields plus every `required` one, followed
 *  by the engine choices the recipe offers. */
export function installFields(fields: ConfigField[], recipe?: Pick<Recipe, "kind" | "startAfterInstall" | "autostart" | "ports" | "secrets">, versionsDir?: string): ConfigField[] {
  return [...fields.filter((f) => f.askOnInstall || f.required), ...engineChoices(recipe, versionsDir)];
}

/** True when a value for `f` already exists — in the tool's stored config
 *  (`has_<key>` for passwords) or in what an API client sent along. */
export function isSettled(f: ConfigField, tool: Pick<ToolRow, "config"> | undefined, request?: Pick<RoadieRequest, "config" | "secretKeys">): boolean {
  if (request) {
    // Passwords and chosen secrets (`secrets.<key>`) the app sent ride privately.
    if (request.secretKeys?.includes(f.key)) return true;
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
