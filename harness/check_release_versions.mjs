import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";

const cargoToml = await readFile("Cargo.toml", "utf8");
const packageJson = JSON.parse(await readFile("packages/lua-rs-wasm/package.json", "utf8"));

const workspaceVersion = matchRequired(
  cargoToml,
  /^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
  "workspace package version",
);

if (packageJson.version !== workspaceVersion) {
  fail(
    `packages/lua-rs-wasm/package.json version ${packageJson.version} does not match workspace ${workspaceVersion}`,
  );
}

const dependencyBlock = matchRequired(
  cargoToml,
  /^\[workspace\.dependencies\]\n([\s\S]*?)(?:\n\[|$)/m,
  "workspace dependencies block",
);

const mismatches = [];
for (const line of dependencyBlock.split(/\r?\n/)) {
  const match = line.match(/^\s*([A-Za-z0-9_-]+)\s*=\s*\{[^}]*version\s*=\s*"([^"]+)"/);
  if (!match) {
    continue;
  }
  const [, name, version] = match;
  if (version !== workspaceVersion) {
    mismatches.push(`${name}=${version}`);
  }
}

for (const mismatch of await localCrateDependencyMismatches(workspaceVersion)) {
  mismatches.push(mismatch);
}

if (mismatches.length > 0) {
  fail(
    `local dependency versions do not match workspace ${workspaceVersion}: ${mismatches.join(", ")}`,
  );
}

console.log(`[versions] workspace/npm versions match ${workspaceVersion}`);

async function localCrateDependencyMismatches(workspaceVersion) {
  const mismatches = [];
  const entries = await readdir("crates", { withFileTypes: true });
  for (const entry of entries) {
    if (!entry.isDirectory()) {
      continue;
    }
    const manifestPath = join("crates", entry.name, "Cargo.toml");
    let manifest;
    try {
      manifest = await readFile(manifestPath, "utf8");
    } catch (err) {
      if (err?.code === "ENOENT") {
        continue;
      }
      throw err;
    }

    for (const line of manifest.split(/\r?\n/)) {
      if (!line.includes("path") || !line.includes("version")) {
        continue;
      }
      const name = line.match(/^\s*([A-Za-z0-9_-]+)\s*=/)?.[1];
      const version = line.match(/\bversion\s*=\s*"([^"]+)"/)?.[1];
      if (name && version && version !== workspaceVersion) {
        mismatches.push(`${manifestPath}:${name}=${version}`);
      }
    }
  }
  return mismatches;
}

function matchRequired(source, pattern, label) {
  const match = source.match(pattern);
  if (!match) {
    fail(`could not read ${label}`);
  }
  return match[1];
}

function fail(message) {
  console.error(`[versions] FAIL: ${message}`);
  process.exit(1);
}
