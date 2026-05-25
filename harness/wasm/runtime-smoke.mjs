import { readFile } from "node:fs/promises";

import { loadLuaRs } from "../../packages/lua-rs-wasm/index.mjs";
import { runRuntimeAssertions, smokeOptions } from "./smoke-scenario.mjs";

const wasmPath =
  process.argv[2] ??
  new URL("../../target/wasm32-unknown-unknown/release/lua_wasm.wasm", import.meta.url);

const { host } = await loadLuaRs(await readFile(wasmPath), smokeOptions());
const result = runRuntimeAssertions(host);

console.log(`wasm32-unknown-unknown runtime smoke ok: ${JSON.stringify(result)}`);
