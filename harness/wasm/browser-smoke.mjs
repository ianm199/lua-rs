import { spawn, spawnSync } from "node:child_process";
import { constants } from "node:fs";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, resolve, sep } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const defaultWasmPath = "/target/wasm32-unknown-unknown/release/lua_wasm.wasm";

const mimeTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".mjs", "text/javascript; charset=utf-8"],
  [".wasm", "application/wasm"],
]);

async function existsExecutable(path) {
  try {
    await access(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

async function findBrowser() {
  if (process.env.LUA_RS_BROWSER) {
    return process.env.LUA_RS_BROWSER;
  }

  const absoluteCandidates = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
  ];
  for (const candidate of absoluteCandidates) {
    if (await existsExecutable(candidate)) {
      return candidate;
    }
  }

  for (const command of ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"]) {
    const result = spawnSync("which", [command], { encoding: "utf8" });
    if (result.status === 0) {
      return result.stdout.trim();
    }
  }

  throw new Error("no Chromium-family browser found; set LUA_RS_BROWSER to run browser smoke");
}

async function removeTempDir(path) {
  await rm(path, {
    force: true,
    maxRetries: 10,
    recursive: true,
    retryDelay: 100,
  });
}

function contentType(path) {
  return mimeTypes.get(extname(path)) ?? "application/octet-stream";
}

async function serveFile(req, res) {
  const requestUrl = new URL(req.url ?? "/", "http://127.0.0.1");
  const pathname =
    requestUrl.pathname === "/" ? "/harness/wasm/browser-smoke.html" : requestUrl.pathname;
  const filePath = resolve(repoRoot, `.${decodeURIComponent(pathname)}`);
  if (filePath !== repoRoot && !filePath.startsWith(`${repoRoot}${sep}`)) {
    res.writeHead(403);
    res.end("forbidden");
    return;
  }

  try {
    const data = await readFile(filePath);
    res.writeHead(200, { "content-type": contentType(filePath) });
    res.end(data);
  } catch (error) {
    res.writeHead(error?.code === "ENOENT" ? 404 : 500);
    res.end(String(error));
  }
}

function listen(server, port = 0) {
  return new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(port, "127.0.0.1", () => resolveListen(server.address()));
  });
}

function close(server) {
  return new Promise((resolveClose, rejectClose) => {
    server.close((error) => (error ? rejectClose(error) : resolveClose()));
  });
}

async function findFreePort() {
  const server = createServer();
  const address = await listen(server);
  await close(server);
  return address.port;
}

async function waitForCdpEndpoint(port) {
  const endpoint = `http://127.0.0.1:${port}/json/list`;
  const deadline = Date.now() + 15_000;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(endpoint);
      if (response.ok) {
        const body = await response.json();
        const page = body.find((target) => target.type === "page" && target.webSocketDebuggerUrl);
        if (page) {
          return page.webSocketDebuggerUrl;
        }
      }
    } catch (error) {
      lastError = error;
    }
    await sleep(100);
  }
  throw new Error(`timed out waiting for Chrome DevTools endpoint: ${lastError ?? "no response"}`);
}

function connectCdp(url) {
  return new Promise((resolveConnect, rejectConnect) => {
    const socket = new WebSocket(url);
    const pending = new Map();
    let nextId = 1;

    socket.addEventListener(
      "open",
      () => {
        socket.addEventListener("message", (event) => {
          const message = JSON.parse(event.data);
          if (!message.id || !pending.has(message.id)) {
            return;
          }
          const { resolve, reject } = pending.get(message.id);
          pending.delete(message.id);
          if (message.error) {
            reject(new Error(JSON.stringify(message.error)));
          } else {
            resolve(message.result);
          }
        });

        resolveConnect({
          send(method, params = {}) {
            const id = nextId++;
            socket.send(JSON.stringify({ id, method, params }));
            return new Promise((resolve, reject) => {
              pending.set(id, { resolve, reject });
            });
          },
          close() {
            socket.close();
          },
        });
      },
      { once: true },
    );
    socket.addEventListener("error", () => rejectConnect(new Error("CDP WebSocket failed")), {
      once: true,
    });
  });
}

async function waitForBrowserSmoke(cdp) {
  const expression = `(() => {
    const node = document.getElementById("status");
    if (!node) {
      return { status: "missing", text: document.documentElement.outerHTML };
    }
    return { status: node.dataset.status || "", text: node.textContent || "" };
  })()`;
  const deadline = Date.now() + 30_000;
  let last = { status: "unknown", text: "" };

  while (Date.now() < deadline) {
    const result = await cdp.send("Runtime.evaluate", {
      awaitPromise: true,
      expression,
      returnByValue: true,
    });
    if (result.exceptionDetails) {
      throw new Error(JSON.stringify(result.exceptionDetails));
    }

    last = result.result.value;
    if (last.status === "pass") {
      return last.text;
    }
    if (last.status === "fail") {
      throw new Error(last.text);
    }

    await sleep(100);
  }

  throw new Error(`timed out waiting for browser smoke status: ${JSON.stringify(last)}`);
}

function launchBrowser(browser, debugPort, userDataDir) {
  const child = spawn(browser, [
    "--headless=new",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--no-first-run",
    "--no-default-browser-check",
    `--user-data-dir=${userDataDir}`,
    `--remote-debugging-port=${debugPort}`,
    "about:blank",
  ]);

  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });

  return { child, stdout: () => stdout, stderr: () => stderr };
}

async function stopBrowser(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  const closed = new Promise((resolveClose) => {
    child.once("close", resolveClose);
  });
  child.kill("SIGTERM");
  const timeout = sleep(5_000).then(() => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
    return closed;
  });
  await Promise.race([closed, timeout]);
}

async function runBrowser(browser, url, userDataDir) {
  const debugPort = await findFreePort();
  const launched = launchBrowser(browser, debugPort, userDataDir);

  try {
    const webSocketUrl = await waitForCdpEndpoint(debugPort);
    const cdp = await connectCdp(webSocketUrl);
    try {
      await cdp.send("Page.enable");
      await cdp.send("Runtime.enable");
      await cdp.send("Page.navigate", { url });
      return await waitForBrowserSmoke(cdp);
    } finally {
      cdp.close();
    }
  } catch (error) {
    error.stdout = launched.stdout();
    error.stderr = launched.stderr();
    throw error;
  } finally {
    await stopBrowser(launched.child);
  }
}

const server = createServer(serveFile);
const browser = await findBrowser();
const userDataDir = await mkdtemp(join(tmpdir(), "lua-rs-browser-smoke-"));

try {
  const address = await listen(server);
  const wasmPath = encodeURIComponent(defaultWasmPath);
  const url = `http://127.0.0.1:${address.port}/harness/wasm/browser-smoke.html?wasm=${wasmPath}`;
  const status = await runBrowser(browser, url, userDataDir);
  console.log(`wasm32-unknown-unknown browser smoke ok: ${status}`);
} catch (error) {
  if (error.stdout) {
    console.error(error.stdout);
  }
  if (error.stderr) {
    console.error(error.stderr);
  }
  throw error;
} finally {
  await close(server);
  await removeTempDir(userDataDir);
}
