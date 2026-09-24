import { describe, expect, it } from "vitest";
import { engineChoices, installFields, isSettled, recipeChangeText, withRequestDecisions } from "../install";
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
    expect(engineChoices(daemon).map((f) => [f.key, f.default])).toEqual([
      ["startNow", true],
      ["autostart", false],
    ]);
    expect(installFields(fields, daemon).map((f) => f.key)).toEqual(["user", "pw", "dir", "startNow", "autostart"]);
    expect(engineChoices({ kind: "daemon", startAfterInstall: { default: true } } as Recipe)).toEqual([]);
    expect(engineChoices({ kind: "cli", autostart: { default: true, askOnInstall: true } } as Recipe)).toEqual([]);
    expect(isSettled(engineChoices(daemon)[0], undefined)).toBe(true);
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

describe("recipeChangeText", () => {
  it("says what an app's own recipe does to Roadie's", () => {
    expect(recipeChangeText("new")).toMatch(/does not know yet/);
    expect(recipeChangeText("replacesBuiltin")).toMatch(/built-in/);
    expect(recipeChangeText("changesTrusted")).toMatch(/you trusted/);
    expect(recipeChangeText("replacesDraft")).toMatch(/draft/);
  });
});
