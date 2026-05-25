# lua-rs-wasm

Browser and JS host wrapper for the `lua-rs` `wasm32-unknown-unknown` runtime.

Install from npm after the package has been published:

```bash
npm install lua-rs-wasm
```

Build the package artifact from the repo:

```bash
npm run build:wasm --prefix packages/lua-rs-wasm
npm test --prefix packages/lua-rs-wasm
npm run test:install --prefix packages/lua-rs-wasm
```

Publish from GitHub Actions with the manual `Publish lua-rs-wasm` workflow. It
runs as a dry-run by default; real publishing requires dispatching from `main`
with `dry_run=false` and an `NPM_TOKEN` repository secret. See
`docs/NPM_WASM_PUBLISHING.md` in the repository root for the full runbook.
After the package exists on npm, `npm run test:registry --prefix
packages/lua-rs-wasm` verifies a fresh install from the public registry.

Instantiate it from a browser or bundler:

```js
import { loadLuaRs, luaRsWasmUrl } from "lua-rs-wasm";

const { lua } = await loadLuaRs(luaRsWasmUrl, {
  env: { LUA_PATH_5_4: "./?.lua" },
  files: {
    "./greeter.lua": "return { message = function(name) return 'hello ' .. name end }",
  },
  stdin: "first line\n",
  unixTime: () => BigInt(Math.floor(Date.now() / 1000)),
  onStdout: (chunk) => console.log(chunk),
});

lua.exec(`
local greeter = require("greeter")
print(greeter.message("wasm"))
`);

const result = lua.tryExec('error("boom")');
console.log(result.ok, result.error);
```

Instantiate it from Node:

```js
import { loadLuaRsNode } from "lua-rs-wasm/node";

const { lua } = await loadLuaRsNode({
  files: {
    "./greeter.lua": "return { message = function(name) return 'hello ' .. name end }",
  },
  onStdout: (chunk) => process.stdout.write(chunk),
});

lua.exec(`
local greeter = require("greeter")
print(greeter.message("node wasm"))
`);
```

The wrapper supplies the `lua_rs_host` imports expected by `lua-wasm`, copies
Lua source into exported WASM memory, runs it through `lua_rs_wasm_run`, exposes
last-error text, and keeps one Lua state alive across `lua.exec(...)` calls until
`lua.reset()` is called.

`luaRsWasmUrl` points at `dist/lua_wasm.wasm`. In browser/bundler contexts,
passing that URL to `loadLuaRs` is the intended path. In Node without a bundler,
use `loadLuaRsNode(...)`, which reads the packaged `.wasm` file and then calls
the same runtime wrapper.
