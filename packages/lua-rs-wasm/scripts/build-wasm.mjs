import { copyFile, mkdir, stat } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const packageRoot = new URL("../", import.meta.url);
const repoRoot = new URL("../../../", import.meta.url);
const wasmSource = new URL(
  "target/wasm32-unknown-unknown/release/lua_wasm.wasm",
  repoRoot,
);
const distDir = new URL("dist/", packageRoot);
const wasmDest = new URL("lua_wasm.wasm", distDir);
const repoRootPath = fileURLToPath(repoRoot);
const wasmSourcePath = fileURLToPath(wasmSource);
const wasmDestPath = fileURLToPath(wasmDest);

const env = { ...process.env };
if (!env.RUSTFLAGS) {
  env.RUSTFLAGS = "-Awarnings";
}

const build = spawnSync(
  "cargo",
  ["build", "--target", "wasm32-unknown-unknown", "-p", "lua-wasm", "--release"],
  {
    cwd: repoRootPath,
    env,
    stdio: "inherit",
  },
);

if (build.status !== 0) {
  process.exit(build.status ?? 1);
}

await mkdir(distDir, { recursive: true });
await copyFile(wasmSource, wasmDest);

const { size } = await stat(wasmDest);
if (size <= 0) {
  throw new Error(`copied empty wasm artifact to ${wasmDestPath}`);
}

console.log(`copied ${wasmSourcePath} -> ${wasmDestPath} (${size} bytes)`);
