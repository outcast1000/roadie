import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTools } from "./hooks/useTools";
import { useRequests } from "./hooks/useRequests";
import { useRecipes } from "./hooks/useRecipes";
import { ToolRow } from "./components/ToolRow";
import { RequestPrompt } from "./components/RequestPrompt";
import { RecipeReview } from "./components/RecipeReview";
import { SettingsPane } from "./components/SettingsPane";
import type { ConsumerPublic } from "./types";
import { matchesQuery } from "./search";

type Tab = "tools" | "settings";

export default function App() {
  const tools = useTools();
  const requests = useRequests();
  const recipes = useRecipes();
  const [tab, setTab] = useState<Tab>("tools");
  const [reviewing, setReviewing] = useState<string | null>(null);
  // Reviewing the recipe a request brought, and which such reviews were opened.
  const [reviewingRequest, setReviewingRequest] = useState<string | null>(null);
  const [reviewedRequests, setReviewedRequests] = useState<Set<string>>(new Set());
  const [highlight, setHighlight] = useState<string | null>(null);
  const [consumers, setConsumers] = useState<ConsumerPublic[]>([]);
  const [intentError, setIntentError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [serviceDown, setServiceDown] = useState<string | null>(null);

  const loadConsumers = useCallback(() => {
    invoke<ConsumerPublic[]>("consumer_list").then(setConsumers).catch((e) => console.error("Failed to list consumers:", e));
  }, []);

  useEffect(() => {
    loadConsumers();
    const un: Array<() => void> = [];
    void listen<{ verb: string; tool: string; known: boolean }>("intent", (ev) => {
      setTab("tools");
      setHighlight(ev.payload.tool);
      window.setTimeout(() => document.getElementById(`tool-${ev.payload.tool}`)?.scrollIntoView({ behavior: "smooth", block: "center" }), 100);
      window.setTimeout(() => setHighlight(null), 6000);
      if (!ev.payload.known) setIntentError(`A link asked for "${ev.payload.tool}", which Roadie has no recipe for.`);
    }).then((u) => un.push(u));
    void listen<{ url: string; error: string }>("intent-error", (ev) => setIntentError(`Ignored a link: ${ev.payload.error}`)).then((u) => un.push(u));
    void listen("tool-status-changed", loadConsumers).then((u) => un.push(u));
    void listen<{ connected: boolean; version?: string }>("service-changed", (ev) => {
      setServiceDown(ev.payload.connected ? null : "The Roadie background service is not answering. Reconnecting…");
      if (ev.payload.connected) {
        void tools.refresh();
        void recipes.refresh();
        void requests.refresh();
      }
    }).then((u) => un.push(u));
    return () => un.forEach((u) => u());
  }, [loadConsumers]);

  const revoke = useCallback(
    async (consumer: string, tool?: string) => {
      try {
        await invoke("consumer_revoke", { id: consumer, tool: tool ?? null });
      } catch (e) {
        console.error("Failed to revoke:", e);
      }
      loadConsumers();
      await tools.refresh();
    },
    [loadConsumers, tools],
  );

  const reviewed = reviewing ? recipes.recipes.find((r) => r.recipe.name === reviewing) : null;
  const broughtRequest = reviewingRequest ? requests.pending.find((r) => r.id === reviewingRequest && r.recipe) : undefined;
  // Stable per request, or the review's dry run would re-run on every render.
  const broughtRecipe = broughtRequest?.recipe;
  const { dryRun: dryRunStored, dryRunRecipe } = recipes;
  const dryRunBroughtReview = useMemo(
    () => (broughtRecipe ? () => dryRunRecipe(broughtRecipe) : dryRunStored),
    [broughtRequest?.id, dryRunRecipe, dryRunStored],
  );
  const visibleTools = tools.tools.filter((t) => matchesQuery(t, query));

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo">R</span>
          <span className="name">Roadie</span>
        </div>
        <nav>
          <button className={tab === "tools" ? "active" : ""} onClick={() => setTab("tools")}>
            Tools
          </button>
          <button className={tab === "settings" ? "active" : ""} onClick={() => setTab("settings")}>
            Settings
          </button>
        </nav>
      </header>

      <main>
        {requests.pending.length > 0 ? (
          <section className="prompts">
            {requests.pending.map((r) => (
              <RequestPrompt
                key={r.id}
                request={r}
                tool={tools.tools.find((t) => t.name === r.tool)}
                fields={recipes.recipes.find((s) => s.recipe.name === r.tool)?.recipe.config ?? []}
                recipe={recipes.recipes.find((s) => s.recipe.name === r.tool)}
                dryRun={recipes.dryRun}
                dryRunRecipe={recipes.dryRunRecipe}
                deciding={!!requests.deciding[r.id]}
                onDecide={(id, approve, answers) => void requests.decide(id, approve, answers)}
                onReview={setReviewing}
                onReviewBrought={(req) => {
                  setReviewingRequest(req.id);
                  setReviewedRequests((s) => new Set(s).add(req.id));
                }}
                broughtReviewed={reviewedRequests.has(r.id)}
              />
            ))}
          </section>
        ) : null}
        {requests.recent
          .filter((r) => r.status === "failed")
          .map((r) => (
            <div key={r.id} className="callout error">
              {r.kind} {r.tool} failed: {r.error}
              <button className="ghost small" onClick={() => requests.dismissRecent(r.id)}>
                Dismiss
              </button>
            </div>
          ))}
        {serviceDown ? (
          <div className="callout warn">
            {serviceDown}
            <button
              className="ghost small"
              onClick={() => void invoke("service_reconnect").then(() => setServiceDown(null)).catch((e) => console.error("Failed to reconnect:", e))}
            >
              Retry now
            </button>
          </div>
        ) : null}
        {intentError ? (
          <div className="callout warn">
            {intentError}
            <button className="ghost small" onClick={() => setIntentError(null)}>
              Dismiss
            </button>
          </div>
        ) : null}
        {recipes.error ? <div className="callout error">{recipes.error}</div> : null}

        {broughtRequest?.recipe && broughtRequest.recipeChange ? (
          <RecipeReview
            stored={{ recipe: broughtRequest.recipe, origin: "draft", submittedBy: broughtRequest.requestedBy }}
            dryRun={dryRunBroughtReview}
            brought={{ requestedBy: broughtRequest.requestedBy, change: broughtRequest.recipeChange }}
            onClose={() => setReviewingRequest(null)}
          />
        ) : reviewed ? (
          <RecipeReview stored={reviewed} dryRun={recipes.dryRun} onTrust={recipes.trust} onDelete={recipes.remove} onClose={() => setReviewing(null)} />
        ) : tab === "tools" ? (
          <>
            {tools.tools.length > 0 ? (
              <div className="search">
                <input
                  type="search"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder="Search recipes — name, author, OS, kind…"
                  aria-label="Search recipes"
                  autoComplete="off"
                />
                {query ? (
                  <button className="ghost small" onClick={() => setQuery("")}>
                    Clear
                  </button>
                ) : null}
                <span className="muted count">
                  {query ? `${visibleTools.length} of ${tools.tools.length}` : `${tools.tools.length} recipe${tools.tools.length === 1 ? "" : "s"}`}
                </span>
              </div>
            ) : null}
            {!tools.loaded ? <p className="muted">Loading…</p> : null}
            {visibleTools.map((t) => (
              <ToolRow
                key={t.name}
                tool={t}
                recipe={recipes.recipes.find((r) => r.recipe.name === t.name)}
                tools={tools}
                highlighted={highlight === t.name}
                onReview={setReviewing}
                onRevoke={revoke}
                dryRun={recipes.dryRun}
              />
            ))}
            {tools.loaded && tools.tools.length === 0 ? <p className="muted">No recipes found.</p> : null}
            {tools.loaded && tools.tools.length > 0 && visibleTools.length === 0 ? (
              <p className="muted">No recipe matches “{query}”. Try a tool name, an author, or an OS such as “windows”.</p>
            ) : null}
          </>
        ) : (
          <SettingsPane consumers={consumers} onRevoke={revoke} />
        )}
      </main>
    </div>
  );
}
