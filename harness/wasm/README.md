# wasm32-unknown-unknown Smoke Harness

This harness proves the bare-WASM runtime path, not the native CLI.

Build the reusable runtime module and instantiate it with Node:

```bash
RUSTFLAGS='-Awarnings' cargo build --target wasm32-unknown-unknown -p lua-wasm --release
node harness/wasm/runtime-smoke.mjs
```

Run the same scenario in a real browser:

```bash
node harness/wasm/browser-smoke.mjs
```

The browser runner starts a local static server and drives Chrome/Chromium
through the DevTools protocol. Set `LUA_RS_BROWSER=/path/to/browser` if the
runner cannot find one automatically.

The lower-level regression crate still exists separately:

```bash
RUSTFLAGS='-Awarnings' cargo build --target wasm32-unknown-unknown -p lua-wasm-smoke --release
node harness/wasm/unknown-smoke.mjs
```

Verify the npm-package shape, including a tarball install into a temporary
project:

```bash
npm run test:install --prefix packages/lua-rs-wasm
```

Run the full WASM/package gate:

```bash
./harness/check_wasm_package.sh
```

Set `WASM_SKIP_BROWSER=1` to skip the Chrome/Chromium smoke on headless systems
that cannot launch a browser.

`packages/lua-rs-wasm` is the reusable JS side of the harness. It has no Node
imports, so the same runtime wrapper can be used from a browser embedder after
it has fetched or bundled the `.wasm` bytes. Its `prepack` script builds
`lua-wasm` and copies `dist/lua_wasm.wasm` into the package. The old
`harness/wasm/lua-rs-host.mjs` path re-exports the package entrypoint for
compatibility.

```js
import { loadLuaRs, luaRsWasmUrl } from "lua-rs-wasm";

const { lua } = await loadLuaRs(luaRsWasmUrl, {
  env: { LUA_PATH_5_4: "./?.lua" },
  files: { "./greeter.lua": "return { answer = function() return 42 end }" },
  stdin: "first line\n",
  unixTime: () => BigInt(Math.floor(Date.now() / 1000)),
  onStdout: (chunk) => console.log(chunk),
});

lua.exec('print(require("greeter").answer())');

const result = lua.tryExec('error("boom")');
console.log(result.ok, result.error);
```

The runtime smoke covers:

- JS-provided Lua source copied into WASM memory and executed by
  `lua_rs_wasm_run`;
- pure Lua compute under `wasm32-unknown-unknown`;
- clean Lua-level failures for unsupported temp-file operations;
- JS-provided host imports for stdout, stdin, env lookup, Unix time, and Lua source
  file loading;
- JS-backed `io.open` read/write/seek/setvbuf file handles;
- JS-backed errno/message propagation for a directory-read failure;
- last-error reporting from a failed Lua chunk back to JS.

The JS host-import bridge uses the `lua_rs_host` import module:

```text
write_stdout(ptr, len) -> i32
read_stdin(out_ptr, out_len) -> i32
unix_time() -> i64
env_len(ptr, len) -> i32
env_read(name_ptr, name_len, out_ptr, out_len) -> i32
file_len(ptr, len) -> i32
file_read(path_ptr, path_len, out_ptr, out_len) -> i32
open_file(path_ptr, path_len, mode_ptr, mode_len) -> i32
file_read_byte(file_id) -> i32
file_write(file_id, ptr, len) -> i32
file_flush(file_id) -> i32
file_seek(file_id, whence, offset) -> i64
file_set_buf_mode(file_id, mode, size) -> i32
file_error_code(file_id) -> i32
file_error_len(file_id) -> i32
file_error_read(file_id, out_ptr, out_len) -> i32
```

The reusable `lua-wasm` module exports:

```text
lua_rs_wasm_alloc(len) -> ptr
lua_rs_wasm_dealloc(ptr, len)
lua_rs_wasm_run(ptr, len) -> i32
lua_rs_wasm_reset() -> i32
lua_rs_wasm_last_error_len() -> usize
lua_rs_wasm_last_error_read(out_ptr, out_len) -> i32
```

The pointer/length pairs reference the module's exported linear memory.
`LuaRsRuntime` copies Lua source into memory, runs it, captures Lua output,
writes stdin/env values back into memory, serves Lua source from a JS `Map`,
opens JS-backed files, and checks that Lua code can call `print`, `io.write`,
`io.read`, `os.getenv`, `os.time`, `require("greeter")`, and `io.open`
read/write/seek/setvbuf operations through those callbacks. It also checks that
successive `lua.exec(...)` calls share one Lua state until `lua.reset()` calls
`lua_rs_wasm_reset`, that reading a JS-hosted directory returns Lua's normal
`false, message, errno` failure tuple, and that a failed Lua chunk exposes its
error message through the last-error exports.

Both Node and browser smoke paths use `smoke-scenario.mjs`, so they exercise the
same Lua behavior through the package entrypoint. The package build produces
`dist/lua_wasm.wasm`; callers can pass that URL in browser/bundler contexts, or
provide bytes/a compiled module directly.
