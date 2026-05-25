#!/usr/bin/env bash
# Lightweight publish-readiness gate for package metadata and package contents.
#
# This does not replace `cargo publish --dry-run` in dependency order. Before
# any internal dependency is on crates.io, Cargo cannot fully verify downstream
# crates because packaged path dependencies are resolved from the registry.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

publishable=(
    lua-gc
    lua-types
    lua-vm
    lua-code
    lua-lex
    lua-stdlib
    lua-coro
    lua-rs-lfs
    lua-parse
    lua-cli
)

echo "[publish] checking release version metadata"
node harness/check_release_versions.mjs

echo "[publish] verifying leaf package: lua-gc"
cargo package -p lua-gc --allow-dirty >/tmp/lua-rs-port-package-lua-gc.log 2>&1

echo "[publish] checking package file lists"
for crate in "${publishable[@]}"; do
    cargo package -p "$crate" --allow-dirty --list >/tmp/lua-rs-port-package-"$crate".list
done

echo "[publish] checking fixture crate is not publishable"
if ! rg -n '^publish = false$' crates/lua-cli-test-rust-module/Cargo.toml >/dev/null; then
    echo "[publish] FAIL: lua-cli-test-rust-module must remain publish = false" >&2
    exit 1
fi

echo "[publish] checking wasm npm package"
WASM_SKIP_BROWSER=1 ./harness/check_wasm_package.sh

echo "[publish] ok"
echo "[publish] note: full verification of dependent crates requires publishing internal deps in order."
