import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const packageRoot = new URL("../", import.meta.url);
const packageRootPath = fileURLToPath(packageRoot);
const tempRoot = await mkdtemp(join(tmpdir(), "lua-rs-wasm-install-"));
const packDir = join(tempRoot, "pack");
const appDir = join(tempRoot, "app");

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    stdio: "pipe",
    encoding: "utf8",
    ...options,
  });
  if (result.status !== 0) {
    throw new Error(
      [
        `${command} ${args.join(" ")} failed with status ${result.status}`,
        result.stdout,
        result.stderr,
      ]
        .filter(Boolean)
        .join("\n"),
    );
  }
  return result;
}

try {
  await mkdirp(packDir);
  await mkdirp(appDir);

  const pack = run("npm", ["pack", packageRootPath, "--pack-destination", packDir], {
    cwd: tempRoot,
  });
  const tarball = pack.stdout
    .trim()
    .split(/\r?\n/)
    .filter(Boolean)
    .at(-1);
  if (!tarball) {
    throw new Error(`npm pack did not report a tarball:\n${pack.stdout}\n${pack.stderr}`);
  }

  const tarballPath = join(packDir, tarball);
  await writeFile(
    join(appDir, "package.json"),
    JSON.stringify({ type: "module", private: true }, null, 2),
  );
  run("npm", ["install", "--silent", tarballPath], { cwd: appDir });

  await writeFile(
    join(appDir, "smoke.mjs"),
`
import { loadLuaRsNode } from "lua-rs-wasm/node";

const { lua } = await loadLuaRsNode({
  env: { LUA_PATH_5_4: "./?.lua" },
  files: { "./installed.lua": "return { value = 14 }" },
  stdin: "installed input\\n",
  unixTime: () => 1700000000n,
});

lua.exec(\`
assert(io.read("l") == "installed input")
assert(os.time() == 1700000000)
local installed = require("installed")
installed_state = installed.value * 3
print("installed package smoke " .. installed_state)
\`);
lua.exec("assert(installed_state == 42)");
lua.reset();
const reset = lua.tryExec("assert(installed_state == nil)");
if (!reset.ok) {
  throw new Error(reset.error);
}
if (!lua.outputText().includes("installed package smoke 42")) {
  throw new Error(lua.outputText());
}
`,
  );
  run("node", ["smoke.mjs"], { cwd: appDir });
  console.log("lua-rs-wasm install smoke ok");
} finally {
  await rm(tempRoot, { recursive: true, force: true });
}

async function mkdirp(path) {
  await mkdir(path, { recursive: true });
}
