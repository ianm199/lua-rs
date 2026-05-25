import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const packageRoot = new URL("../", import.meta.url);
const packageJson = JSON.parse(await readFile(new URL("package.json", packageRoot), "utf8"));
const specifier = process.argv[2] ?? `${packageJson.name}@${packageJson.version}`;
const attempts = Number(process.env.LUA_RS_WASM_REGISTRY_SMOKE_ATTEMPTS ?? "12");
const delayMs = Number(process.env.LUA_RS_WASM_REGISTRY_SMOKE_DELAY_MS ?? "5000");
const tempRoot = await mkdtemp(join(tmpdir(), "lua-rs-wasm-registry-"));
const appDir = join(tempRoot, "app");

function run(command, args, options = {}) {
  return spawnSync(command, args, {
    stdio: "pipe",
    encoding: "utf8",
    ...options,
  });
}

function requireSuccess(result, label) {
  if (result.status !== 0) {
    throw new Error(
      [
        `${label} failed with status ${result.status}`,
        result.stdout,
        result.stderr,
      ]
        .filter(Boolean)
        .join("\n"),
    );
  }
  return result;
}

async function installWithRetries() {
  let last;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    last = run("npm", ["install", "--silent", specifier], { cwd: appDir });
    if (last.status === 0) {
      return;
    }
    if (attempt < attempts) {
      console.error(
        `npm install ${specifier} failed on attempt ${attempt}/${attempts}; retrying in ${delayMs}ms`,
      );
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
  requireSuccess(last, `npm install ${specifier}`);
}

try {
  await mkdir(appDir, { recursive: true });
  await writeFile(
    join(appDir, "package.json"),
    JSON.stringify({ type: "module", private: true }, null, 2),
  );
  await installWithRetries();

  await writeFile(
    join(appDir, "smoke.mjs"),
`
import { loadLuaRsNode } from "lua-rs-wasm/node";

const { lua } = await loadLuaRsNode({
  env: { LUA_PATH_5_4: "./?.lua" },
  files: { "./registry.lua": "return { value = 6 }" },
  stdin: "registry input\\n",
  unixTime: () => 1700000000n,
});

lua.exec(\`
assert(io.read("l") == "registry input")
assert(os.time() == 1700000000)
local registry = require("registry")
registry_state = registry.value * 7
print("registry package smoke " .. registry_state)
\`);
lua.exec("assert(registry_state == 42)");
lua.reset();
const reset = lua.tryExec("assert(registry_state == nil)");
if (!reset.ok) {
  throw new Error(reset.error);
}
if (!lua.outputText().includes("registry package smoke 42")) {
  throw new Error(lua.outputText());
}
`,
  );
  requireSuccess(run("node", ["smoke.mjs"], { cwd: appDir }), "node smoke.mjs");
  console.log(`lua-rs-wasm registry smoke ok (${specifier})`);
} finally {
  await rm(tempRoot, { recursive: true, force: true });
}
