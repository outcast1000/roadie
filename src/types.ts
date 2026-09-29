// Mirrors of the Rust types the window receives. Keep in step with
// src-tauri/src/tools/mod.rs (ToolStatus), recipe/mod.rs, requests.rs.

export type Kind = "daemon" | "cli";
/** `builtin` no longer occurs (Roadie ships no recipes); `catalog` is offered by the recipe catalog, not trusted. */
export type Origin = "builtin" | "user" | "draft" | "catalog";
/** Where a trusted user recipe came from. */
export type RecipeSource = "user" | "catalog";
export type ConnectionPolicy = "none" | "open" | "perConsumerKey" | "sharedKey";
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
  /** Where the installed release is unpacked; null until installed. */
  installDir: string | null;
  /** An install running now, from any process: phase `resolving`, then `downloading`/`extracting`/`verifying`. */
  installing: { phase: string; downloaded: number; total: number | null; updatedAt: number } | null;
  /** Where releases are unpacked (the chosen install folder, or Roadie's default), installed or not. */
  versionsDir: string;
  /** False once the tool owns its configuration (written once at install): Roadie's settings no longer apply. */
  configurable: boolean;
  /** The tool's private data dir (state, secrets, rendered config). */
  dataDir: string;
  logsDir: string;
  /** The recipe's config files, paths only; `secret` marks one holding secrets. */
  configFiles: { path: string; secret: boolean }[];
  origin: Origin;
  trusted: boolean;
  source: RecipeSource;
  /** The recipe catalog offers this tool. */
  available: boolean;
  /** A newer catalog revision of the trusted recipe, waiting for review. */
  recipeUpdate: { revision: number; changedKeys: string[] } | null;
  /** A trusted catalog recipe the catalog no longer lists. */
  delisted: boolean;
  submittedBy: string | null;
}

/** `GET /v1/recipes/{name}/submission`: proposing a recipe to the catalog. */
export interface Submission {
  name: string;
  repo: string;
  path: string;
  file: string;
  isUpdate: boolean;
  revision: number;
  catalogRevision: number | null;
  submitUrl: string;
  clipboardFallback: boolean;
  instructions: string;
}

export type FieldKind = "text" | "password" | "path" | "paths" | "bool" | "port";

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

/** A setting of the tool's own config file, named by its real key there
 *  (`shares.directories`); Roadie writes the value into the file. Stored and
 *  set under `entry`, so as a form field its key is the entry path. */
export interface ConfigEntry {
  entry: string;
  label: string;
  help?: string | null;
  kind: FieldKind;
  required?: boolean;
  default?: unknown;
  askOnInstall?: boolean;
  merge?: "replace" | "append";
  file?: string | null;
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
  configuration?: ConfigEntry[];
  ports?: Record<string, { default: number; pick?: boolean; askOnInstall?: boolean; label?: string | null }>;
  /** No `generate`: required at install. */
  secrets?: { key: string; generate?: string | null; askOnInstall?: boolean; label?: string | null; minLen?: number | null }[];
  connection?: { policy: ConnectionPolicy; url: string; key?: string | null } | null;
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
  source: RecipeSource;
  submittedBy: string | null;
}

export interface RenderedFile {
  path: string;
  secret: boolean;
  contents: string;
}

/** Another copy of the tool already running here (`tools::other_instance`). */
export interface OtherInstance {
  url: string;
  /** The recipe is a `singleton`: Roadie's copy will not start while it runs. */
  blocksStart: boolean;
  message: string;
}

export interface DryRun {
  /** Another copy already running here, which Roadie's would clash with. */
  otherInstance?: OtherInstance | null;
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
  /** `catalog` when `recipe` is the recipe catalog's (a first install or a recipe update). */
  recipeSource?: RecipeSource;
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
