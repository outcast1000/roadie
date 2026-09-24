// Pure helpers behind the Tools tab's search box and the platform chips.
// No React here so vitest can cover them directly.

import type { ToolRow } from "./types";

const OS_NAMES: Record<string, string> = { darwin: "macOS", windows: "Windows", linux: "Linux" };
const ARCH_NAMES: Record<string, string> = { arm64: "ARM", x64: "Intel/AMD" };

/** `darwin-arm64` → `macOS (ARM)`; unknown keys are shown as written. */
export function platformLabel(key: string): string {
  const [os, arch] = key.split("-");
  const osName = OS_NAMES[os];
  const archName = ARCH_NAMES[arch];
  if (!osName || !archName) return key;
  return `${osName} (${archName})`;
}

/** One chip per OS: `macOS`, `Windows (ARM)`, … Both architectures collapse
 *  into the bare OS name so a card reads "macOS · Windows · Linux". */
export function platformChips(keys: string[]): string[] {
  const byOs = new Map<string, Set<string>>();
  for (const k of keys) {
    const [os, arch] = k.split("-");
    if (!byOs.has(os)) byOs.set(os, new Set());
    byOs.get(os)!.add(arch);
  }
  const order = ["darwin", "windows", "linux"];
  return [...byOs.entries()]
    .sort((a, b) => (order.indexOf(a[0]) === -1 ? 99 : order.indexOf(a[0])) - (order.indexOf(b[0]) === -1 ? 99 : order.indexOf(b[0])))
    .map(([os, arches]) => {
      const osName = OS_NAMES[os] ?? os;
      if (arches.size >= 2) return osName;
      const arch = [...arches][0];
      return `${osName} (${ARCH_NAMES[arch] ?? arch})`;
    });
}

/** Everything a search term can hit for a tool, lower-cased. */
export function searchText(t: Pick<ToolRow, "name" | "displayName" | "author" | "summary" | "kind" | "origin" | "platforms" | "notes">): string {
  const chips = platformChips(t.platforms);
  return [
    t.name,
    t.displayName,
    t.author,
    t.summary,
    t.notes ?? "",
    t.kind === "daemon" ? "daemon background service" : "cli command-line tool",
    t.origin,
    t.origin === "user" ? "your recipe" : "",
    ...t.platforms,
    ...chips,
    ...t.platforms.map(platformLabel),
  ]
    .join(" ")
    .toLowerCase();
}

/** Every whitespace-separated word of `query` must appear somewhere in the
 *  tool's searchable text. An empty query matches everything. */
export function matchesQuery(t: Parameters<typeof searchText>[0], query: string): boolean {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const hay = searchText(t);
  return words.every((w) => hay.includes(w));
}
