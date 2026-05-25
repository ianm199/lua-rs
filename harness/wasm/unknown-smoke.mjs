import { readFile } from "node:fs/promises";

import { instantiateLuaRs } from "../../packages/lua-rs-wasm/index.mjs";
import { runSmokeAssertions, smokeOptions } from "./smoke-scenario.mjs";

const wasmPath =
  process.argv[2] ??
  new URL("../../target/wasm32-unknown-unknown/release/lua_wasm_smoke.wasm", import.meta.url);

const { exports, host } = await instantiateLuaRs(await readFile(wasmPath), smokeOptions());
const { outputBytes } = runSmokeAssertions(exports, host);

console.log(`wasm32-unknown-unknown smoke ok: outputBytes=${outputBytes}`);
