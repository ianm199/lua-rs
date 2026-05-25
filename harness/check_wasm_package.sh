#!/usr/bin/env bash
# End-to-end gate for the bare wasm32 runtime and JS package wrapper.
#
# This checks the consumer-shaped path, not just compilation:
#   - wasm crates check for wasm32-unknown-unknown
#   - the npm package builds and packs dist/lua_wasm.wasm
#   - the package can be installed from a tarball and imported by package name
#   - the low-level wasm smoke artifact is built from a clean checkout
#   - Node and browser smokes execute Lua through the package entrypoint

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

export RUSTFLAGS="${RUSTFLAGS:--Awarnings}"

echo "[wasm] checking JS syntax"
node --check harness/check_release_versions.mjs
node --check packages/lua-rs-wasm/index.mjs
node --check packages/lua-rs-wasm/node.mjs
node --check packages/lua-rs-wasm/scripts/build-wasm.mjs
node --check packages/lua-rs-wasm/scripts/smoke.mjs
node --check packages/lua-rs-wasm/scripts/install-smoke.mjs
node --check packages/lua-rs-wasm/scripts/registry-smoke.mjs
node --check harness/wasm/runtime-smoke.mjs
node --check harness/wasm/unknown-smoke.mjs
node --check harness/wasm/browser-smoke.mjs

echo "[wasm] checking release version metadata"
node harness/check_release_versions.mjs

echo "[wasm] checking Rust crates for wasm32-unknown-unknown"
cargo check --target wasm32-unknown-unknown \
  -p lua-rs-runtime \
  -p lua-wasm \
  -p lua-wasm-smoke

echo "[wasm] building packaged wasm artifact"
npm run build:wasm --prefix packages/lua-rs-wasm

echo "[wasm] building low-level wasm smoke artifact"
cargo build --target wasm32-unknown-unknown -p lua-wasm-smoke --release

echo "[wasm] checking generated wasm git hygiene"
if ! git check-ignore -q packages/lua-rs-wasm/dist/lua_wasm.wasm; then
  echo "[wasm] FAIL: generated dist/lua_wasm.wasm should be ignored by git" >&2
  exit 1
fi

echo "[wasm] running package smoke"
npm test --prefix packages/lua-rs-wasm

echo "[wasm] running tarball install smoke"
npm run test:install --prefix packages/lua-rs-wasm

echo "[wasm] checking npm package contents"
PACK_LOG="$(mktemp)"
PACK_JSON="$(mktemp)"
trap 'rm -f "$PACK_LOG" "$PACK_JSON"' EXIT
npm pack --dry-run --json ./packages/lua-rs-wasm >"$PACK_LOG"
awk 'found || /^\[/ { found=1; print }' "$PACK_LOG" >"$PACK_JSON"
node -e '
const fs = require("node:fs");
const path = process.argv[1];
const payload = JSON.parse(fs.readFileSync(path, "utf8"));
const files = payload?.[0]?.files ?? [];
const byPath = new Map(files.map((file) => [file.path, file]));
for (const required of [
  "dist/lua_wasm.wasm",
  "index.mjs",
  "node.mjs",
  "package.json",
  "scripts/build-wasm.mjs",
  "scripts/install-smoke.mjs",
  "scripts/registry-smoke.mjs",
  "scripts/smoke.mjs",
]) {
  if (!byPath.has(required)) {
    throw new Error(`npm pack file list missing ${required}`);
  }
}
const wasm = byPath.get("dist/lua_wasm.wasm");
if (!wasm.size || wasm.size <= 0) {
  throw new Error("npm pack file list contains empty dist/lua_wasm.wasm");
}
' "$PACK_JSON"
rm -f "$PACK_LOG" "$PACK_JSON"
trap - EXIT

echo "[wasm] running Node runtime smoke"
node harness/wasm/runtime-smoke.mjs

echo "[wasm] running low-level wasm smoke"
node harness/wasm/unknown-smoke.mjs

if [ "${WASM_SKIP_BROWSER:-0}" = "1" ]; then
  echo "[wasm] skipping browser smoke because WASM_SKIP_BROWSER=1"
else
  echo "[wasm] running browser smoke"
  node harness/wasm/browser-smoke.mjs
fi

echo "[wasm] ok"
