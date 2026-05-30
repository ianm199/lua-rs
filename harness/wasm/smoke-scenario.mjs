export function smokeOptions() {
  return {
    env: new Map([
      ["LUA_PATH_5_4", "./hosted-54/?.lua"],
      ["LUA_PATH", "./hosted/?.lua"],
    ]),
    files: new Map([
      [
        "./hosted-54/greeter.lua",
        `
return {
  answer = function() return 42 end,
  message = function(name) return "hello " .. name end,
}
`,
      ],
      ["./hosted-54/data.txt", "alpha\nbeta\n"],
    ]),
    dirs: ["./hosted-54/dir"],
    stdin: "alpha\nbeta\n",
  };
}

export function runRuntimeAssertions(host) {
  host.clearSetBufCalls();
  host.exec(`
assert(os.getenv("LUA_PATH") == "./hosted/?.lua")
assert(package.path == "./hosted-54/?.lua")
assert(os.time() == 1700000000)
assert(io.read("l") == "alpha")
assert(io.read("l") == "beta")
assert(io.read("l") == nil)
local tmp_ok = pcall(function() return os.tmpname() end)
assert(tmp_ok == false)
local tmpfile, tmp_err = io.tmpfile()
assert(tmpfile == false and type(tmp_err) == "string")
local greeter = require("greeter")
assert(greeter.answer() == 42)
assert(greeter.message("wasm") == "hello wasm")
local f = assert(io.open("./hosted-54/data.txt", "r"))
assert(f:read(5) == "alpha")
assert(f:seek("cur") == 5)
assert(f:seek("set", 6) == 6)
assert(f:read("a") == "beta\\n")
assert(f:seek("end", -5) == 6)
assert(f:read("a") == "beta\\n")
assert(f:close())
local dir = assert(io.open("./hosted-54/dir", "r"))
local ok, msg, errno = dir:read("a")
assert(ok == false)
assert(type(msg) == "string")
assert(errno == 21)
assert(dir:close())
local out = assert(io.open("./hosted-54/runtime.txt", "w"))
assert(out:setvbuf("no"))
assert(out:write("runtime ", greeter.answer()))
assert(out:setvbuf("full", 32))
assert(out:seek("set", 0) == 0)
assert(out:setvbuf("line", 16))
assert(out:write("runtime ", greeter.answer()))
assert(out:flush())
assert(out:close())
print("runtime print")
io.write("runtime io\\n")
`);

  if (!host.outputText().includes("runtime print") || !host.outputText().includes("runtime io")) {
    throw new Error(`runtime output missing expected text: ${JSON.stringify(host.outputText())}`);
  }
  if (host.readFile("./hosted-54/runtime.txt") !== "runtime 42") {
    throw new Error(
      `runtime file output mismatch: ${JSON.stringify(host.readFile("./hosted-54/runtime.txt"))}`,
    );
  }

  const setBufModes = host.setBufModeSummary();
  if (setBufModes !== "0:8192,1:32,2:16") {
    throw new Error(`runtime setvbuf calls mismatch: ${setBufModes}`);
  }

  host.exec("wasm_state = { value = 41 }");
  host.exec(`
wasm_state.value = wasm_state.value + 1
assert(wasm_state.value == 42)
print("stateful runtime " .. wasm_state.value)
`);
  if (!host.outputText().includes("stateful runtime 42")) {
    throw new Error(`stateful runtime output missing: ${JSON.stringify(host.outputText())}`);
  }

  host.reset();
  const afterReset = host.tryExec("assert(wasm_state == nil)");
  if (!afterReset.ok) {
    throw new Error(`runtime reset did not clear Lua globals: ${afterReset.error}`);
  }

  host.exec("");

  const failed = host.tryExec('error("expected wasm failure")');
  if (failed.ok) {
    throw new Error("expected failing Lua script to return an error");
  }
  if (!failed.error.includes("expected wasm failure")) {
    throw new Error(`last error missing Lua failure text: ${JSON.stringify(failed.error)}`);
  }

  // Sandbox ABI: an instruction budget aborts a runaway script — uncatchable,
  // even wrapped in a pcall loop — and the trip reason is reported. setLimits
  // resets the runtime, so this runs last.
  host.setLimits({ maxInstructions: 200000 });
  const runaway = host.tryExec(
    "while true do pcall(function() while true do end end) end",
  );
  if (runaway.ok) {
    throw new Error("sandboxed runaway script should have aborted");
  }
  if (host.lastTrip() !== "instructions") {
    throw new Error(`expected instruction trip, got ${host.lastTrip()}`);
  }

  return { setBufModes };
}

export function runSmokeAssertions(exports, host) {
  const pure = exports.lua_rs_wasm_pure_compute_smoke();
  if (pure !== 1) {
    throw new Error(`pure compute smoke failed: ${pure}`);
  }

  const outputBytes = exports.lua_rs_wasm_output_hook_smoke();
  if (outputBytes <= 0) {
    throw new Error(`output hook smoke failed: ${outputBytes}`);
  }

  const unsupported = exports.lua_rs_wasm_unsupported_host_smoke();
  if (unsupported !== 1) {
    throw new Error(`unsupported host smoke failed: ${unsupported}`);
  }

  const input = exports.lua_rs_wasm_input_hook_smoke();
  if (input !== 1) {
    throw new Error(`input hook smoke failed: ${input}`);
  }

  const env = exports.lua_rs_wasm_env_hook_smoke();
  if (env !== 1) {
    throw new Error(`env hook smoke failed: ${env}`);
  }

  const jsHost = exports.lua_rs_wasm_js_host_hook_smoke();
  if (jsHost !== 1) {
    throw new Error(`JS host hook smoke failed: ${jsHost}`);
  }
  if (!host.outputText().includes("js host print") || !host.outputText().includes("js host io")) {
    throw new Error(`JS host output missing expected text: ${JSON.stringify(host.outputText())}`);
  }
  if (host.readFile("./hosted-54/out.txt") !== "fromWASM hello file") {
    throw new Error(
      `JS host file output mismatch: ${JSON.stringify(host.readFile("./hosted-54/out.txt"))}`,
    );
  }

  const setBufModes = host.setBufModeSummary();
  if (setBufModes !== "0:8192,1:32,2:16") {
    throw new Error(`JS host setvbuf calls mismatch: ${setBufModes}`);
  }

  const dynamic = host.run(`
local greeter = require("greeter")
local out = assert(io.open("./hosted-54/dynamic.txt", "w"))
assert(out:write("dynamic ", greeter.answer()))
assert(out:flush())
assert(out:close())
print("dynamic script")
`);
  if (dynamic !== 1) {
    throw new Error(`dynamic JS-provided Lua script failed: ${dynamic}`);
  }
  if (!host.outputText().includes("dynamic script")) {
    throw new Error(`dynamic script output missing: ${JSON.stringify(host.outputText())}`);
  }
  if (host.readFile("./hosted-54/dynamic.txt") !== "dynamic 42") {
    throw new Error(
      `dynamic script file output mismatch: ${JSON.stringify(host.readFile("./hosted-54/dynamic.txt"))}`,
    );
  }

  return { outputBytes, setBufModes };
}
