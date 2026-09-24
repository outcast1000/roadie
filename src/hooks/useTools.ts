import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { InstallProgress, ToolRow } from "../types";

export interface ToolsHook {
  tools: ToolRow[];
  loaded: boolean;
  busy: Record<string, string>;
  installing: Record<string, InstallProgress>;
  errors: Record<string, string>;
  refresh: () => Promise<void>;
  refreshOne: (name: string) => Promise<void>;
  install: (name: string, config?: Record<string, unknown>) => Promise<void>;
  update: (name: string) => Promise<void>;
  start: (name: string) => Promise<void>;
  stop: (name: string) => Promise<void>;
  restart: (name: string) => Promise<void>;
  setAutostart: (name: string, enabled: boolean) => Promise<void>;
  configure: (name: string, patch: Record<string, unknown>) => Promise<void>;
  uninstall: (name: string, keepData: boolean) => Promise<void>;
  checkUpdates: (name: string) => Promise<void>;
  logs: (name: string, lines?: number) => Promise<string>;
  openLogs: (name: string) => Promise<void>;
  clearError: (name: string) => void;
}

export function useTools(): ToolsHook {
  const [tools, setTools] = useState<ToolRow[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState<Record<string, string>>({});
  const [installing, setInstalling] = useState<Record<string, InstallProgress>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const refreshTimer = useRef<number | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<ToolRow[]>("tool_list");
      setTools(list);
    } catch (e) {
      console.error("Failed to list tools:", e);
    } finally {
      setLoaded(true);
    }
  }, []);

  const refreshOne = useCallback(async (name: string) => {
    try {
      const row = await invoke<ToolRow>("tool_status", { name });
      setTools((prev) => (prev.some((t) => t.name === name) ? prev.map((t) => (t.name === name ? row : t)) : [...prev, row]));
    } catch (e) {
      console.error(`Failed to read ${name}:`, e);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const unlisten: Array<() => void> = [];
    void listen<{ name: string }>("tool-status-changed", () => {
      // Coalesce bursts (a start emits several) into one list read.
      if (refreshTimer.current) window.clearTimeout(refreshTimer.current);
      refreshTimer.current = window.setTimeout(() => void refresh(), 150);
    }).then((u) => unlisten.push(u));
    void listen<InstallProgress>("tool-install-progress", (ev) => {
      setInstalling((prev) => ({ ...prev, [ev.payload.name]: ev.payload }));
    }).then((u) => unlisten.push(u));
    void listen("recipe-changed", () => void refresh()).then((u) => unlisten.push(u));
    // Daemons change state on their own (a crash, a slow start): poll gently.
    const poll = window.setInterval(() => void refresh(), 15000);
    return () => {
      unlisten.forEach((u) => u());
      window.clearInterval(poll);
    };
  }, [refresh]);

  const run = useCallback(
    async (name: string, label: string, op: () => Promise<unknown>) => {
      setBusy((b) => ({ ...b, [name]: label }));
      setErrors((e) => {
        const next = { ...e };
        delete next[name];
        return next;
      });
      try {
        await op();
      } catch (e) {
        console.error(`${label} ${name} failed:`, e);
        setErrors((prev) => ({ ...prev, [name]: String(e) }));
      } finally {
        setBusy((b) => {
          const next = { ...b };
          delete next[name];
          return next;
        });
        setInstalling((p) => {
          const next = { ...p };
          delete next[name];
          return next;
        });
        await refresh();
      }
    },
    [refresh],
  );

  return {
    tools,
    loaded,
    busy,
    installing,
    errors,
    refresh,
    refreshOne,
    install: (name, config) => run(name, "Installing", () => invoke("tool_install", { name, config: config ?? null })),
    update: (name) => run(name, "Updating", () => invoke("tool_update", { name })),
    start: (name) => run(name, "Starting", () => invoke("tool_start", { name })),
    stop: (name) => run(name, "Stopping", () => invoke("tool_stop", { name })),
    restart: (name) => run(name, "Restarting", () => invoke("tool_restart", { name })),
    setAutostart: (name, enabled) => run(name, "Saving", () => invoke("tool_set_autostart", { name, enabled })),
    configure: (name, patch) => run(name, "Saving", () => invoke("tool_configure", { name, patch })),
    uninstall: (name, keepData) => run(name, "Removing", () => invoke("tool_uninstall", { name, keepData })),
    checkUpdates: (name) => run(name, "Checking", () => invoke("tool_check_updates", { name })),
    logs: (name, lines) => invoke<string>("tool_logs", { name, lines: lines ?? 60 }),
    openLogs: (name) => invoke("tool_open_logs", { name }),
    clearError: (name) =>
      setErrors((e) => {
        const next = { ...e };
        delete next[name];
        return next;
      }),
  };
}
