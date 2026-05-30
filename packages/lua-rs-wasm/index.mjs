const decoder = new TextDecoder();
const encoder = new TextEncoder();

export const luaRsWasmUrl = new URL("./dist/lua_wasm.wasm", import.meta.url);

function asMap(value) {
  if (value instanceof Map) {
    return new Map(value);
  }
  if (Array.isArray(value)) {
    return new Map(value);
  }
  if (value && typeof value === "object") {
    return new Map(Object.entries(value));
  }
  return new Map();
}

function asSet(value) {
  if (value instanceof Set) {
    return new Set(value);
  }
  if (Array.isArray(value)) {
    return new Set(value);
  }
  return new Set();
}

function asBytes(value) {
  if (value instanceof Uint8Array) {
    return value;
  }
  return encoder.encode(String(value));
}

export class LuaRsRuntime {
  constructor({ env, files, dirs, stdin = "", unixTime = 1700000000n, onStdout } = {}) {
    this.env = asMap(env);
    this.files = asMap(files);
    this.dirs = asSet(dirs);
    this.stdin = Array.from(asBytes(stdin));
    this.stdinOffset = 0;
    this.unixTime = unixTime;
    this.onStdout = onStdout;
    this.instance = undefined;
    this.stdout = "";
    this.nextFileId = 1;
    this.openFiles = new Map();
    this.setBufCalls = [];
  }

  get imports() {
    return {
      lua_rs_host: {
        write_stdout: (ptr, len) => this.writeStdout(ptr, len),
        read_stdin: (outPtr, outLen) => this.readStdin(outPtr, outLen),
        unix_time: () => this.readUnixTime(),
        env_len: (ptr, len) => this.envLen(ptr, len),
        env_read: (namePtr, nameLen, outPtr, outLen) =>
          this.envRead(namePtr, nameLen, outPtr, outLen),
        file_len: (ptr, len) => this.fileLen(ptr, len),
        file_read: (pathPtr, pathLen, outPtr, outLen) =>
          this.fileRead(pathPtr, pathLen, outPtr, outLen),
        open_file: (pathPtr, pathLen, modePtr, modeLen) =>
          this.openFile(pathPtr, pathLen, modePtr, modeLen),
        file_read_byte: (id) => this.fileReadByte(id),
        file_write: (id, ptr, len) => this.fileWrite(id, ptr, len),
        file_flush: (id) => this.fileFlush(id),
        file_seek: (id, whence, offset) => this.fileSeek(id, whence, offset),
        file_set_buf_mode: (id, mode, size) => this.fileSetBufMode(id, mode, size),
        file_error_code: (id) => this.fileErrorCode(id),
        file_error_len: (id) => this.fileErrorLen(id),
        file_error_read: (id, outPtr, outLen) => this.fileErrorRead(id, outPtr, outLen),
      },
    };
  }

  attach(instance) {
    this.instance = instance;
    return this;
  }

  memoryBytes() {
    if (!this.instance) {
      throw new Error("LuaRsRuntime is not attached to a WebAssembly instance");
    }
    return new Uint8Array(this.instance.exports.memory.buffer);
  }

  outputText() {
    return this.stdout;
  }

  resetOutput() {
    this.stdout = "";
  }

  consumeOutput() {
    const output = this.stdout;
    this.resetOutput();
    return output;
  }

  setStdin(source) {
    this.stdin = Array.from(asBytes(source));
    this.stdinOffset = 0;
  }

  appendStdin(source) {
    this.stdin.push(...asBytes(source));
  }

  setEnv(name, value) {
    this.env.set(name, value);
  }

  deleteEnv(name) {
    return this.env.delete(name);
  }

  readFile(path) {
    const value = this.files.get(path);
    return value === undefined ? undefined : decoder.decode(asBytes(value));
  }

  writeFile(path, source) {
    this.files.set(path, source);
  }

  deleteFile(path) {
    return this.files.delete(path);
  }

  mkdir(path) {
    this.dirs.add(path);
  }

  readTextFile(path) {
    return this.readFile(path);
  }

  writeTextFile(path, source) {
    this.writeFile(path, source);
  }

  addDir(path) {
    this.mkdir(path);
  }

  setBufModeSummary() {
    return this.setBufCalls.map((call) => `${call.mode}:${call.size}`).join(",");
  }

  clearSetBufCalls() {
    this.setBufCalls = [];
  }

  lastErrorText() {
    const exports = this.instance?.exports;
    if (!exports?.lua_rs_wasm_last_error_len || !exports?.lua_rs_wasm_last_error_read) {
      return "";
    }

    const len = exports.lua_rs_wasm_last_error_len();
    if (len <= 0) {
      return "";
    }

    const ptr = exports.lua_rs_wasm_alloc(len);
    if (ptr === 0) {
      throw new Error("lua_rs_wasm_alloc returned null for last error buffer");
    }
    try {
      const written = exports.lua_rs_wasm_last_error_read(ptr, len);
      if (written < 0) {
        throw new Error("lua_rs_wasm_last_error_read failed");
      }
      return decoder.decode(this.memoryBytes().subarray(ptr, ptr + written));
    } finally {
      exports.lua_rs_wasm_dealloc(ptr, len);
    }
  }

  run(source, { throwOnError = true } = {}) {
    if (!this.instance) {
      throw new Error("LuaRsRuntime is not attached to a WebAssembly instance");
    }
    const exports = this.instance.exports;
    const run = exports.lua_rs_wasm_run ?? exports.lua_rs_wasm_run_hosted_script;
    if (!run) {
      throw new Error("WASM module does not export a lua-rs run function");
    }

    const sourceBytes = asBytes(source);
    const ptr = exports.lua_rs_wasm_alloc(sourceBytes.length);
    if (ptr === 0 && sourceBytes.length !== 0) {
      throw new Error("lua_rs_wasm_alloc returned null");
    }
    try {
      this.memoryBytes().set(sourceBytes, ptr);
      const status = run(ptr, sourceBytes.length);
      if (status !== 1 && throwOnError) {
        const message = this.lastErrorText();
        throw new Error(message || `Lua execution failed with status ${status}`);
      }
      return status;
    } finally {
      exports.lua_rs_wasm_dealloc(ptr, sourceBytes.length);
    }
  }

  exec(source) {
    this.run(source, { throwOnError: true });
    return this;
  }

  tryExec(source) {
    const status = this.run(source, { throwOnError: false });
    if (status === 1) {
      return { ok: true };
    }
    return { ok: false, error: this.lastErrorText() };
  }

  reset() {
    if (!this.instance) {
      throw new Error("LuaRsRuntime is not attached to a WebAssembly instance");
    }
    const reset = this.instance.exports.lua_rs_wasm_reset;
    if (!reset) {
      throw new Error("WASM module does not export lua_rs_wasm_reset");
    }
    const status = reset();
    if (status !== 1) {
      const message = this.lastErrorText();
      throw new Error(message || `Lua reset failed with status ${status}`);
    }
    return this;
  }

  /**
   * Bound CPU and memory for subsequent `run`/`exec` calls and (optionally)
   * strip host-access globals, for running untrusted scripts. Resets the
   * runtime so the limits take effect on a fresh state. A `0` / omitted limit
   * means unlimited; `strict` defaults the limits to 10M instructions / 64 MiB
   * and removes `os.execute`, `io`, `load`, `require`, `debug`, …
   */
  setLimits({ maxInstructions = 0, maxMemory = 0, strict = false } = {}) {
    if (!this.instance) {
      throw new Error("LuaRsRuntime is not attached to a WebAssembly instance");
    }
    const setLimits = this.instance.exports.lua_rs_wasm_set_limits;
    if (!setLimits) {
      throw new Error("WASM module does not export lua_rs_wasm_set_limits");
    }
    const status = setLimits(
      BigInt(maxInstructions),
      BigInt(maxMemory),
      strict ? 1 : 0,
    );
    if (status !== 1) {
      const message = this.lastErrorText();
      throw new Error(message || `Lua set_limits failed with status ${status}`);
    }
    return this;
  }

  /**
   * Which sandbox limit, if any, aborted the most recent run:
   * "instructions", "memory", or null (no trip / ordinary error).
   */
  lastTrip() {
    const lastTrip = this.instance?.exports?.lua_rs_wasm_last_trip;
    if (!lastTrip) {
      return null;
    }
    switch (lastTrip()) {
      case 1:
        return "instructions";
      case 2:
        return "memory";
      default:
        return null;
    }
  }

  /** Refill the instruction budget and clear the trip flag without recreating
   * the runtime. */
  sandboxReset() {
    const sandboxReset = this.instance?.exports?.lua_rs_wasm_sandbox_reset;
    if (sandboxReset) {
      sandboxReset();
    }
    return this;
  }

  readString(ptr, len) {
    return decoder.decode(this.memoryBytes().subarray(ptr, ptr + len));
  }

  writeBytes(ptr, outLen, bytes) {
    if (bytes.length > outLen) {
      return -1;
    }
    this.memoryBytes().set(bytes, ptr);
    return bytes.length;
  }

  writeStdout(ptr, len) {
    const chunk = this.readString(ptr, len);
    this.stdout += chunk;
    if (this.onStdout) {
      this.onStdout(chunk);
    }
    return 0;
  }

  readStdin(outPtr, outLen) {
    if (outLen <= 0 || this.stdinOffset >= this.stdin.length) {
      return 0;
    }
    const count = Math.min(outLen, this.stdin.length - this.stdinOffset);
    const chunk = this.stdin.slice(this.stdinOffset, this.stdinOffset + count);
    this.stdinOffset += count;
    this.memoryBytes().set(chunk, outPtr);
    return count;
  }

  readUnixTime() {
    const value = typeof this.unixTime === "function" ? this.unixTime() : this.unixTime;
    return BigInt(value);
  }

  envLen(ptr, len) {
    const value = this.env.get(this.readString(ptr, len));
    return value === undefined ? -1 : asBytes(value).length;
  }

  envRead(namePtr, nameLen, outPtr, outLen) {
    const value = this.env.get(this.readString(namePtr, nameLen));
    return value === undefined ? -1 : this.writeBytes(outPtr, outLen, asBytes(value));
  }

  fileLen(ptr, len) {
    const value = this.files.get(this.readString(ptr, len));
    return value === undefined ? -1 : asBytes(value).length;
  }

  fileRead(pathPtr, pathLen, outPtr, outLen) {
    const value = this.files.get(this.readString(pathPtr, pathLen));
    return value === undefined ? -1 : this.writeBytes(outPtr, outLen, asBytes(value));
  }

  openFile(pathPtr, pathLen, modePtr, modeLen) {
    const path = this.readString(pathPtr, pathLen);
    const mode = this.readString(modePtr, modeLen);
    const id = this.nextFileId++;

    if (mode.startsWith("r")) {
      if (this.dirs.has(path)) {
        this.openFiles.set(id, {
          mode,
          path,
          data: [],
          pos: 0,
          readError: { code: 21, message: "is a directory" },
        });
        return id;
      }

      const source = this.files.get(path);
      if (source === undefined) {
        return -1;
      }
      this.openFiles.set(id, {
        mode,
        path,
        data: Array.from(asBytes(source)),
        pos: 0,
      });
      return id;
    }

    if (mode.startsWith("w")) {
      this.files.set(path, "");
      this.openFiles.set(id, {
        mode,
        path,
        data: [],
        pos: 0,
      });
      return id;
    }

    if (mode.startsWith("a")) {
      const data = Array.from(asBytes(this.files.get(path) ?? ""));
      this.openFiles.set(id, {
        mode,
        path,
        data,
        pos: data.length,
      });
      return id;
    }

    return -1;
  }

  fileReadByte(id) {
    const file = this.openFiles.get(id);
    if (file === undefined || !file.mode.startsWith("r")) {
      return -2;
    }
    if (file.readError !== undefined) {
      return -2;
    }
    if (file.pos >= file.data.length) {
      return -1;
    }
    return file.data[file.pos++];
  }

  fileWrite(id, ptr, len) {
    const file = this.openFiles.get(id);
    if (file === undefined || (!file.mode.startsWith("w") && !file.mode.startsWith("a"))) {
      return -1;
    }
    const chunk = this.memoryBytes().slice(ptr, ptr + len);
    for (const byte of chunk) {
      file.data[file.pos++] = byte;
    }
    return chunk.length;
  }

  fileFlush(id) {
    const file = this.openFiles.get(id);
    if (file === undefined) {
      return -1;
    }
    if (file.mode.startsWith("w") || file.mode.startsWith("a")) {
      this.files.set(file.path, decoder.decode(Uint8Array.from(file.data)));
    }
    return 0;
  }

  fileSeek(id, whence, offset) {
    const file = this.openFiles.get(id);
    if (file === undefined) {
      return -1n;
    }

    const numericOffset = Number(offset);
    let next;
    if (whence === 0) {
      next = numericOffset;
    } else if (whence === 1) {
      next = file.pos + numericOffset;
    } else if (whence === 2) {
      next = file.data.length + numericOffset;
    } else {
      return -1n;
    }

    if (!Number.isSafeInteger(next) || next < 0) {
      return -1n;
    }
    file.pos = next;
    return BigInt(next);
  }

  fileSetBufMode(id, mode, size) {
    if (!this.openFiles.has(id)) {
      return -1;
    }
    this.setBufCalls.push({ id, mode, size });
    return 0;
  }

  fileErrorCode(id) {
    const file = this.openFiles.get(id);
    return file?.readError?.code ?? 0;
  }

  fileErrorLen(id) {
    const message = this.openFiles.get(id)?.readError?.message;
    return message === undefined ? 0 : asBytes(message).length;
  }

  fileErrorRead(id, outPtr, outLen) {
    const message = this.openFiles.get(id)?.readError?.message;
    return message === undefined ? 0 : this.writeBytes(outPtr, outLen, asBytes(message));
  }
}

export const LuaRsHost = LuaRsRuntime;

export async function instantiateLuaRs(bytesOrModule, options) {
  const host = new LuaRsRuntime(options);
  const result = await WebAssembly.instantiate(bytesOrModule, host.imports);
  const instance = result instanceof WebAssembly.Instance ? result : result.instance;
  const module = result instanceof WebAssembly.Instance ? undefined : result.module;
  host.attach(instance);
  return { lua: host, host, instance, module, exports: instance.exports };
}

async function resolveWasmSource(source) {
  if (typeof source === "string" || source instanceof URL) {
    const response = await fetch(source);
    if (!response.ok) {
      throw new Error(`failed to fetch wasm: ${response.status} ${response.statusText}`);
    }
    return response.arrayBuffer();
  }
  if (typeof Response !== "undefined" && source instanceof Response) {
    if (!source.ok) {
      throw new Error(`failed to fetch wasm: ${source.status} ${source.statusText}`);
    }
    return source.arrayBuffer();
  }
  return source;
}

export async function loadLuaRs(wasmSource, options) {
  return instantiateLuaRs(await resolveWasmSource(wasmSource), options);
}
