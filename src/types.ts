// Mirrors of the Rust types the window receives. Keep in step with
// src-tauri/src/tools/mod.rs (ToolStatus), recipe/mod.rs, requests.rs.

export type Kind = "daemon" | "cli";
export type Origin = "builtin" | "user" | "draft";
export type ConnectionPolicy = "none" | "open" | "perConsumerKey";
export type DeferReason = "busy" | "unreachable" | "restartNotAllowed";

export interface ToolRow {
  name: string;
  displayName: string;
  author: string;
  revision: number;
  platforms: string[];
  summary: string;
  kind: Kind;
  notes: string | null;
  homepage: string | null;
  supported: boolean;
  installed: boolean;
  version: string | null;
  latest: string | null;
  updateAvailable: boolean;
  updateStaged: string | null;
  updateDeferredReason: DeferReason | null;
  restartPending: boolean;
  running: boolean;
  healthy: boolean;
  starting: boolean;
  pid: number | null;
  conflict: string | null;
  conflictDetail: string | null;
  autostart: boolean;
  url: string | null;
  binPath: string | null;
  connectionPolicy: ConnectionPolicy;
  /** The recipe declares a web-page login; fetch it with `tool_web_login`. */
  hasWebLogin: boolean;
  approvedConsumers: string[];
  config: Record<string, unknown>;
  details: Record<string, unknown>;
  reportedVersion: string | null;
  healthDetail: string | null;
  logsDir: string;
  origin: Origin;
  trusted: boolean;
  submittedBy: string | null;
}

export type FieldKind = "text" | "password" | "path" | "bool" | "port";

export interface ConfigField {
  key: string;
  label: string;
  help?: string | null;
  kind: FieldKind;
  required?: boolean;
  secret?: boolean;
  default?: unknown;
  tccSensitive?: boolean;
  createDir?: boolean;
  /** Ask for this before the first install (required fields are always asked). */
  askOnInstall?: boolean;
}

export interface InstallChoice {
  default: boolean;
  askOnInstall?: boolean;
}

export interface Recipe {
  recipeVersion: number;
  name: string;
  displayName: string;
  author: string;
  revision: number;
  platforms: string[];
  summary: string;
  homepage?: string | null;
  license?: string | null;
  notes?: string | null;
  kind: Kind;
  source: Record<string, unknown> & { kind: string };
  archive: string;
  layout?: { stripTopDir?: boolean; binaries?: string[] };
  config: ConfigField[];
  ports?: Record<string, { default: number; pick?: boolean }>;
  connection?: { policy: ConnectionPolicy; url: string } | null;
  createDirs?: string[];
  files?: { path: string; format: string; secret?: boolean; content: unknown }[];
  run?: { args: string[]; env?: Record<string, string>; cwd?: string | null } | null;
  health?: unknown;
  stop?: unknown;
  /** Daemon only: engine-owned install choices (decision keys `startNow`, `autostart`). */
  startAfterInstall?: InstallChoice | null;
  autostart?: InstallChoice | null;
}

export interface StoredRecipe {
  recipe: Recipe;
  origin: Origin;
  submittedBy: string | null;
}

export interface RenderedFile {
  path: string;
  secret: boolean;
  contents: string;
}

export interface DryRun {
  platform: string;
  supported: boolean;
  resolved: { version: string; downloadUrl: string; asset: string; checksumsUrl: string | null; floating: boolean } | null;
  resolveError: string | null;
  assetReachable: boolean | null;
  files: RenderedFile[];
  runArgs: string[];
  createDirs: string[];
  connectionUrl: string | null;
  installDir: string;
  dataDir: string;
  logsDir: string;
  binPath: string | null;
  ports: Record<string, number>;
}

export type RequestStatus = "pending" | "approved" | "declined" | "done" | "failed";

/** How a recipe an app brought differs from what Roadie has (`store::Change`). */
export type RecipeChange = "new" | "replacesBuiltin" | "changesTrusted" | "replacesDraft";

export interface RoadieRequest {
  id: string;
  kind: "install" | "uninstall" | "connect" | "replaceRecipe";
  tool: string;
  consumer?: string;
  keepData?: boolean;
  /** Install: the client's non-secret decisions; `secretKeys` names the passwords it supplied. */
  config?: Record<string, unknown>;
  secretKeys?: string[];
  /** Install / replaceRecipe: the recipe the app brought, reviewed and trusted by approving. */
  recipe?: Recipe;
  recipeChange?: RecipeChange;
  returnUrl?: string;
  requestedBy: string;
  status: RequestStatus;
  createdAt: number;
  error: string | null;
  progress: { phase: string; downloaded: number; total: number | null } | null;
}

export interface ConsumerPublic {
  id: string;
  displayName: string;
  returnPrefix: string | null;
  builtin: boolean;
  tools: string[];
}

export interface Settings {
  autoUpdateTools: boolean;
  /** On: the service is a login item and outlives the window. Off: plain app, service exits with the window. */
  runInBackground: boolean;
}

export interface AppInfo {
  version: string;
  dataDir: string;
  apiPort: number | null;
  platform: string;
  /** The background service this window is connected to; null while unreachable. */
  service: { version: string; pid: number; ownerChannel: boolean; loginItem: boolean } | null;
}

export interface McpSetupInfo {
  scriptPath: string | null;
  nodePath: string | null;
  nodeVersion: string | null;
  nodeOk: boolean;
  dataDir: string;
  problem: string | null;
}

export interface InstallProgress {
  name: string;
  phase: "downloading" | "extracting" | "verifying";
  downloaded: number;
  total: number | null;
}
