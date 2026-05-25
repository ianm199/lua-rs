import { readFile } from "node:fs/promises";

import { loadLuaRs } from "../index.mjs";

const wasmPath = new URL("../dist/lua_wasm.wasm", import.meta.url);

const { lua } = await loadLuaRs(await readFile(wasmPath), {
  env: { LUA_PATH_5_4: "./?.lua" },
  files: {
    "./pkg.lua": "return { value = 21 }",
  },
  stdin: "input line\n",
  unixTime: () => 1700000000n,
});

lua.exec(`
assert(io.read("l") == "input line")
assert(os.time() == 1700000000)
local pkg = require("pkg")
wasm_package_state = { value = pkg.value * 2 }
print("package smoke " .. wasm_package_state.value)
`);

lua.exec("assert(wasm_package_state.value == 42)");

lua.reset();
const reset = lua.tryExec("assert(wasm_package_state == nil)");
if (!reset.ok) {
  throw new Error(`reset did not clear Lua state: ${reset.error}`);
}

if (!lua.outputText().includes("package smoke 42")) {
  throw new Error(`missing package smoke output: ${JSON.stringify(lua.outputText())}`);
}

console.log("lua-rs-wasm package smoke ok");
