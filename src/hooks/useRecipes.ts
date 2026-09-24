import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { DryRun, StoredRecipe } from "../types";

export function useRecipes() {
  const [recipes, setRecipes] = useState<StoredRecipe[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setRecipes(await invoke<StoredRecipe[]>("recipe_list"));
    } catch (e) {
      console.error("Failed to list recipes:", e);
    }
  }, []);

  useEffect(() => {
    void refresh();
    let unlisten: (() => void) | null = null;
    void listen("recipe-changed", () => void refresh()).then((u) => (unlisten = u));
    return () => {
      if (unlisten) unlisten();
    };
  }, [refresh]);

  const trust = useCallback(
    async (name: string) => {
      setError(null);
      try {
        await invoke("recipe_trust", { name });
      } catch (e) {
        console.error("Failed to trust recipe:", e);
        setError(String(e));
      }
      await refresh();
    },
    [refresh],
  );

  const remove = useCallback(
    async (name: string) => {
      setError(null);
      try {
        await invoke("recipe_delete", { name });
      } catch (e) {
        console.error("Failed to delete recipe:", e);
        setError(String(e));
      }
      await refresh();
    },
    [refresh],
  );

  const dryRun = useCallback(async (name: string) => invoke<DryRun>("recipe_dry_run", { name }), []);

  return { recipes, drafts: recipes.filter((r) => r.origin === "draft"), error, refresh, trust, remove, dryRun };
}
