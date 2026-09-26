import { describe, expect, it } from "vitest";
import { describeDefer, formatBytes, stateLabel } from "../components/ToolRow";
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
  logsDir: "",
  origin: "builtin",
  trusted: true,
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
