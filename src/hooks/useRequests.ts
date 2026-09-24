import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { RoadieRequest } from "../types";

/** Prompts waiting for the user: install/uninstall/connect requests from
 *  API clients and deep links. */
export function useRequests() {
  const [pending, setPending] = useState<RoadieRequest[]>([]);
  const [deciding, setDeciding] = useState<Record<string, boolean>>({});
  const [recent, setRecent] = useState<RoadieRequest[]>([]);

  const refresh = useCallback(async () => {
    try {
      setPending(await invoke<RoadieRequest[]>("request_list"));
    } catch (e) {
      console.error("Failed to list requests:", e);
    }
  }, []);

  useEffect(() => {
    void refresh();
    let unlisten: (() => void) | null = null;
    void listen<RoadieRequest>("request-changed", (ev) => {
      const r = ev.payload;
      if (r.status === "pending") {
        setPending((p) => (p.some((x) => x.id === r.id) ? p.map((x) => (x.id === r.id ? r : x)) : [...p, r]));
      } else {
        setPending((p) => p.filter((x) => x.id !== r.id));
        setRecent((prev) => [r, ...prev.filter((x) => x.id !== r.id)].slice(0, 5));
      }
    }).then((u) => (unlisten = u));
    return () => {
      if (unlisten) unlisten();
    };
  }, [refresh]);

  /** The user's click. `answers` are the install decisions typed or changed
   *  in the prompt; they win over what the client sent. */
  const decide = useCallback(async (id: string, approve: boolean, answers?: Record<string, unknown>) => {
    setDeciding((d) => ({ ...d, [id]: true }));
    try {
      const r = await invoke<RoadieRequest>("request_decide", { id, approve, answers: answers ?? null });
      setPending((p) => p.filter((x) => x.id !== id));
      setRecent((prev) => [r, ...prev.filter((x) => x.id !== r.id)].slice(0, 5));
    } catch (e) {
      console.error("Failed to decide request:", e);
    } finally {
      setDeciding((d) => {
        const next = { ...d };
        delete next[id];
        return next;
      });
    }
  }, []);

  return { pending, recent, deciding, decide, refresh, dismissRecent: (id: string) => setRecent((r) => r.filter((x) => x.id !== id)) };
}
