#!/usr/bin/env node
// Roadie's version lives in five files; this keeps them in step.
//
//   node scripts/version.mjs 0.2.0          set it everywhere
//   node scripts/version.mjs --check 0.2.0  exit 1 unless every file says 0.2.0 (CI runs this)
//   node scripts/version.mjs                print what each file says
//
// Both releases (desktop and CLI) are built from the same crate, so they
// share this number. A release tag is `desktop-v<version>` or
// `cli-v<version>` and must match it (see RELEASING.md).

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

const files = {
  "package.json": {
    read: (t) => JSON.parse(t).version,
    write: (t, v) => topLevel(t, v),
  },
  "package-lock.json": {
    read: (t) => {
      const j = JSON.parse(t);
      return j.version === j.packages?.[""]?.version ? j.version : `${j.version} / ${j.packages?.[""]?.version}`;
    },
    // The top-level version, then the root package's under "packages" → "".
    write: (t, v) => topLevel(t, v).replace(/(\n    "": \{[^}]*?\n      "version": ")[^"]+(")/, `$1${v}$2`),
  },
  "src-tauri/tauri.conf.json": {
    read: (t) => JSON.parse(t).version,
    write: (t, v) => topLevel(t, v),
  },
  "src-tauri/Cargo.toml": {
    // The first `version =` inside [package].
    read: (t) => t.match(/\[package\][^[]*?\nversion = "([^"]+)"/)?.[1],
    write: (t, v) => t.replace(/(\[package\][^[]*?\nversion = ")[^"]+(")/, `$1${v}$2`),
  },
  "src-tauri/Cargo.lock": {
    read: (t) => t.match(/\nname = "roadie"\nversion = "([^"]+)"/)?.[1],
    write: (t, v) => t.replace(/(\nname = "roadie"\nversion = ")[^"]+(")/, `$1${v}$2`),
  },
};

// Edit the line in place so the file keeps its own formatting.
function topLevel(text, v) {
  return text.replace(/(\n  "version": ")[^"]+(")/, `$1${v}$2`);
}

function current() {
  return Object.fromEntries(Object.entries(files).map(([f, h]) => [f, h.read(fs.readFileSync(path.join(root, f), "utf8")) ?? "(not found)"]));
}

const args = process.argv.slice(2);
if (args.length === 0) {
  for (const [f, v] of Object.entries(current())) console.log(`${v.padEnd(12)} ${f}`);
} else if (args[0] === "--check") {
  const want = args[1];
  if (!want || !SEMVER.test(want)) {
    console.error(`usage: node scripts/version.mjs --check <x.y.z>; got ${want ?? "nothing"}`);
    process.exit(2);
  }
  const wrong = Object.entries(current()).filter(([, v]) => v !== want);
  if (wrong.length) {
    for (const [f, v] of wrong) console.error(`${f} says ${v}, the tag says ${want}`);
    console.error(`run: node scripts/version.mjs ${want}  (then commit, and tag again)`);
    process.exit(1);
  }
  console.log(`every version file says ${want}`);
} else {
  const v = args[0];
  if (!SEMVER.test(v)) {
    console.error(`not a version: ${v} (expected x.y.z or x.y.z-pre)`);
    process.exit(2);
  }
  for (const [f, h] of Object.entries(files)) {
    const p = path.join(root, f);
    const before = fs.readFileSync(p, "utf8");
    const after = h.write(before, v);
    if (h.read(after) !== v) {
      console.error(`could not set the version in ${f}`);
      process.exit(1);
    }
    fs.writeFileSync(p, after);
  }
  console.log(`version set to ${v} in ${Object.keys(files).join(", ")}`);
}
