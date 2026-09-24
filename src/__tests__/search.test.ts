import { describe, expect, it } from "vitest";
import { matchesQuery, platformChips, platformLabel } from "../search";

const slskd = {
  name: "slskd",
  displayName: "slskd",
  author: "Roadie",
  summary: "Soulseek client with a web UI and HTTP API.",
  notes: null,
  kind: "daemon" as const,
  origin: "builtin" as const,
  platforms: ["darwin-arm64", "darwin-x64", "windows-x64", "windows-arm64", "linux-x64"],
};
const ffmpeg = { ...slskd, name: "ffmpeg", displayName: "FFmpeg", author: "Jane Doe", summary: "Converts audio and video.", kind: "cli" as const, origin: "draft" as const, platforms: ["darwin-arm64", "windows-x64", "windows-arm64"] };

describe("platform chips", () => {
  it("names platforms for humans", () => {
    expect(platformLabel("darwin-arm64")).toBe("macOS (ARM)");
    expect(platformLabel("linux-x64")).toBe("Linux (Intel/AMD)");
    expect(platformLabel("plan9-mips")).toBe("plan9-mips");
  });
  it("collapses both architectures into the OS and keeps a lone one visible", () => {
    expect(platformChips(slskd.platforms)).toEqual(["macOS", "Windows", "Linux (Intel/AMD)"]);
    expect(platformChips(ffmpeg.platforms)).toEqual(["macOS (ARM)", "Windows"]);
    expect(platformChips([])).toEqual([]);
  });
});

describe("matchesQuery", () => {
  it("matches name, author, OS, kind and origin, case-insensitively, all words required", () => {
    expect(matchesQuery(slskd, "")).toBe(true);
    expect(matchesQuery(slskd, "SLSKD")).toBe(true);
    expect(matchesQuery(slskd, "soulseek")).toBe(true);
    expect(matchesQuery(ffmpeg, "jane")).toBe(true);
    expect(matchesQuery(slskd, "jane")).toBe(false);
    expect(matchesQuery(slskd, "linux")).toBe(true);
    expect(matchesQuery(ffmpeg, "linux")).toBe(false);
    expect(matchesQuery(ffmpeg, "windows arm")).toBe(true);
    expect(matchesQuery(slskd, "daemon")).toBe(true);
    expect(matchesQuery(ffmpeg, "command-line")).toBe(true);
    expect(matchesQuery(ffmpeg, "draft")).toBe(true);
    expect(matchesQuery(slskd, "macos soulseek")).toBe(true);
    expect(matchesQuery(slskd, "macos nothing-like-this")).toBe(false);
  });
});
