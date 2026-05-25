import { readFile } from "node:fs/promises";

export {
  LuaRsHost,
  LuaRsRuntime,
  instantiateLuaRs,
  loadLuaRs,
  luaRsWasmUrl,
} from "./index.mjs";

import { loadLuaRs, luaRsWasmUrl } from "./index.mjs";

export async function loadLuaRsNode(options = {}, wasmPath = luaRsWasmUrl) {
  return loadLuaRs(await readFile(wasmPath), options);
}
