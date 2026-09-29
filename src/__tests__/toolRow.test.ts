import { describe, expect, it } from "vitest";
import { describeDefer, formatBytes, originBadge, stateLabel } from "../components/ToolRow";
import type { ToolRow } from "../types";

const base: ToolRow = {
  name: "slskd",
  displayName: "slskd",
  author: "Roadie",
  revision: 1,
  platforms: ["darwin-arm64", "windows-x64"],
  summary: "",
  kind: "daemon",
  notes: null,
  homepage: null,
  supported: true,
  installed: true,
  version: "0.26.0",
  latest: null,
  updateAvailable: false,
  updateStaged: null,
  updateDeferredReason: null,
  restartPending: false,
  running: false,
  healthy: false,
  starting: false,
  pid: null,
  conflict: null,
  conflictDetail: null,
  autostart: false,
  url: "http://127.0.0.1:5030",
  binPath: null,
  connectionPolicy: "perConsumerKey",
  hasWebLogin: true,
  approvedConsumers: [],
  config: {},
  details: {},
  reportedVersion: null,
  healthDetail: null,
  installDir: null,
  installing: null,
  versionsDir: "",
  configurable: true,
  dataDir: "",
  logsDir: "",
  configFiles: [],
  origin: "user",
  trusted: true,
  source: "catalog",
  available: true,
  recipeUpdate: null,
  delisted: false,
  submittedBy: null,
};

describe("stateLabel", () => {
  it("orders the states the way the patch did", () => {
    expect(stateLabel({ ...base, trusted: false, origin: "draft" }).tone).toBe("warn");
    expect(stateLabel({ ...base, supported: false }).text).toMatch(/Not available/);
    expect(stateLabel({ ...base, installed: false }).text).toBe("Not installed");
    expect(stateLabel({ ...base, kind: "cli" }).text).toBe("Installed · 0.26.0");
    expect(stateLabel({ ...base, conflict: "foreignInstanceOnPort" }).tone).toBe("error");
    expect(stateLabel({ ...base, conflict: "standaloneRunning", conflictDetail: "Another copy…" }).text).toBe("Another copy…");
    expect(stateLabel({ ...base, running: true, healthy: true, reportedVersion: "0.26.0.0" }).text).toBe("Running · 0.26.0.0");
    expect(stateLabel({ ...base, running: true, starting: true }).text).toBe("Starting…");
    expect(stateLabel({ ...base, running: true }).tone).toBe("warn");
    expect(stateLabel(base).text).toBe("Stopped · 0.26.0");
    expect(stateLabel({ ...base, origin: "catalog", trusted: false, installed: false }).text).toMatch(/Available/);
    expect(stateLabel({ ...base, origin: "catalog", trusted: false, supported: false }).text).toMatch(/Not available/);
  });

  it("badges drafts, catalog offers and the user's own recipes, not catalog ones they trusted", () => {
    expect(originBadge({ origin: "catalog", source: "catalog" })).toBe("catalog");
    expect(originBadge({ origin: "draft", source: "user" })).toBe("draft");
    expect(originBadge({ origin: "user", source: "user" })).toBe("your recipe");
    expect(originBadge({ origin: "user", source: "catalog" })).toBeNull();
  });
});

describe("helpers", () => {
  it("formats bytes and defer reasons", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(58309223)).toBe("55.6 MB");
    expect(describeDefer("busy")).toMatch(/idle/);
    expect(describeDefer("restartNotAllowed")).toMatch(/next start/);
  });
});
