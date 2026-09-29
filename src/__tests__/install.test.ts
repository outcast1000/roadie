import { describe, expect, it } from "vitest";
import { engineChoices, installFields, isSettled, recipeChangeText, recipeFields, recipeOriginText, withRequestDecisions } from "../install";
import type { ConfigField, Recipe, ToolRow } from "../types";

const fields: ConfigField[] = [
  { key: "user", label: "User", kind: "text", askOnInstall: true },
  { key: "pw", label: "Password", kind: "password", secret: true, askOnInstall: true },
  { key: "dir", label: "Folder", kind: "path", required: true },
  { key: "share", label: "Share", kind: "bool" },
];
const tool = { config: { dir: "/x", has_pw: false } } as unknown as ToolRow;

describe("install decisions", () => {
  it("asks for askOnInstall and required fields only", () => {
    expect(installFields(fields).map((f) => f.key)).toEqual(["user", "pw", "dir"]);
  });
  it("appends the engine choices a daemon recipe offers", () => {
    const daemon = { kind: "daemon", startAfterInstall: { default: true, askOnInstall: true }, autostart: { default: false, askOnInstall: true } } as Recipe;
    expect(engineChoices(daemon, "/data/tools/x/versions").map((f) => [f.key, f.default])).toEqual([
      ["installDir", "/data/tools/x/versions"],
      ["startNow", true],
      ["autostart", false],
    ]);
    expect(installFields(fields, daemon).map((f) => f.key)).toEqual(["user", "pw", "dir", "installDir", "startNow", "autostart"]);
    expect(engineChoices({ kind: "daemon", startAfterInstall: { default: true } } as Recipe).map((f) => f.key)).toEqual(["installDir"]);
    expect(engineChoices({ kind: "cli", autostart: { default: true, askOnInstall: true } } as Recipe).map((f) => f.key)).toEqual(["installDir"]);
    expect(isSettled(engineChoices(daemon).find((f) => f.key === "startNow")!, undefined)).toBe(true);
  });
  it("offers the ports and secrets a recipe lets the app or user choose, with their defaults", () => {
    const r = {
      kind: "daemon",
      ports: { web: { default: 5030, askOnInstall: true, label: "Web port" }, listen: { default: 50300 } },
      secrets: [{ key: "internalKey", generate: "hex48", askOnInstall: true, label: "API key", minLen: 16 }],
    } as unknown as Recipe;
    const got = engineChoices(r, "/v");
    expect(got.map((f) => [f.key, f.kind, f.default ?? null])).toEqual([
      ["installDir", "path", "/v"],
      ["ports.web", "port", 5030],
      ["secrets.internalKey", "text", null],
    ]);
    expect(got[2].help).toMatch(/generates one/);
    expect(isSettled(got[2], undefined, { config: {}, secretKeys: ["secrets.internalKey"] })).toBe(true);
  });
  it("knows what is settled from the tool and from a request", () => {
    expect(isSettled(fields[0], tool)).toBe(false);
    expect(isSettled(fields[2], tool)).toBe(true);
    expect(isSettled(fields[1], tool)).toBe(false);
    expect(isSettled(fields[1], tool, { secretKeys: ["pw"] })).toBe(true);
    expect(isSettled(fields[0], tool, { config: { user: "bj" } })).toBe(true);
    expect(isSettled(fields[0], tool, { config: { user: "" } })).toBe(false);
    expect(isSettled(fields[0], undefined)).toBe(false);
  });
  it("overlays a request's decisions onto the tool config for the form", () => {
    const merged = withRequestDecisions({ ...tool, config: { dir: "/x" } }, { config: { user: "bj" }, secretKeys: ["pw"] });
    expect(merged.config).toEqual({ dir: "/x", user: "bj", has_pw: true });
  });
});

describe("recipeOriginText", () => {
  it("names the catalog as the source, and the app otherwise", () => {
    expect(recipeOriginText("Viboplr", "new", "catalog")).toMatch(/Roadie recipe catalog/);
    expect(recipeOriginText("Viboplr", "changesTrusted", "catalog")).toMatch(/new revision/);
    expect(recipeOriginText("Viboplr", "new")).toBe("Viboplr brings a recipe for a tool Roadie does not know yet");
  });
});

describe("recipeChangeText", () => {
  it("says what an app's own recipe does to Roadie's", () => {
    expect(recipeChangeText("new")).toMatch(/does not know yet/);
    expect(recipeChangeText("replacesBuiltin")).toMatch(/built-in/);
    expect(recipeChangeText("changesTrusted")).toMatch(/you trusted/);
    expect(recipeChangeText("replacesDraft")).toMatch(/draft/);
  });
});

describe("recipeFields", () => {
  it("adds configuration entries as fields keyed by their entry path, after config", () => {
    const fields = recipeFields({
      config: [{ key: "downloadsDir", label: "Downloads folder", kind: "path" }],
      configuration: [{ entry: "shares.directories", label: "Also share these folders", kind: "paths", default: [], askOnInstall: true, merge: "append" }],
    });
    expect(fields.map((f) => f.key)).toEqual(["downloadsDir", "shares.directories"]);
    expect(fields[1]).toMatchObject({ kind: "paths", askOnInstall: true, default: [] });
    expect(installFields(fields).map((f) => f.key)).toEqual(["shares.directories"]);
  });

  it("is empty without a recipe and plain config without entries", () => {
    expect(recipeFields(undefined)).toEqual([]);
    expect(recipeFields({ config: [{ key: "a", label: "A", kind: "text" }] }).map((f) => f.key)).toEqual(["a"]);
  });
});
