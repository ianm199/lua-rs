# Publishing to crates.io

`lua-rs` ships as a 10-crate workspace. Publishing is **manual and irreversible**
— a crates.io version can never be overwritten, only yanked — so this is a
human-run checklist, not an automated step.

## Current state

- Workspace version: **0.0.2** (set in `Cargo.toml` `[workspace.package]`).
- Published on crates.io: **0.0.1** (frozen before the lua.c CLI + REPL + recent
  fixes). 0.0.2 is what this checklist publishes.
- `crates/lua-cli-test-rust-module` is `publish = false` — never published.

## Pre-publish checks

```bash
cargo build --workspace --bins                       # compiles clean
TEST_TIMEOUT_S=90 ./harness/run_official_all.sh       # 44/44 PASS
.claude/hooks/unsafe-budget.sh                        # unsafe budget OK
git status                                            # clean tree
```

Confirm you are logged in (`cargo login` with a token from
https://crates.io/settings/tokens) and that the version in `Cargo.toml` has been
bumped beyond what's already on crates.io.

## Publish order (dependency-topological)

Each crate must be on crates.io before anything that depends on it, or
`cargo publish` fails with "no matching package". Publish in exactly this order:

```bash
cargo publish -p lua-gc
cargo publish -p lua-types
cargo publish -p lua-vm
cargo publish -p lua-coro
cargo publish -p lua-code
cargo publish -p lua-lex
cargo publish -p lua-rs-lfs
cargo publish -p lua-stdlib
cargo publish -p lua-parse
cargo publish -p lua-cli
```

The internal dependency graph this order satisfies:

| Crate | Depends on (internal) |
|---|---|
| `lua-gc` | — |
| `lua-types` | lua-gc |
| `lua-vm` | lua-gc, lua-types |
| `lua-coro` | lua-types |
| `lua-code` | lua-types, lua-vm |
| `lua-lex` | lua-types, lua-vm |
| `lua-rs-lfs` | lua-types, lua-vm |
| `lua-stdlib` | lua-types, lua-vm |
| `lua-parse` | lua-code, lua-lex, lua-types, lua-vm |
| `lua-cli` | lua-parse, lua-rs-lfs, lua-stdlib, lua-types, lua-vm |

## Indexing & rate limits

- After each `cargo publish`, crates.io needs a few seconds to index the new
  version before the next dependent crate can resolve it. Recent cargo waits and
  retries automatically; if a publish fails with "no matching package", wait
  ~30s and rerun that one crate.
- crates.io rate-limits new-version publishes (roughly one per minute for a new
  account). If you hit a limit, pause and continue — the order still holds.
- Dry-run any crate first if unsure: `cargo publish -p <crate> --dry-run`.

## After publishing

```bash
git tag v0.0.2 && git push origin v0.0.2

# verify the installed crate end-to-end
cargo install lua-cli --version 0.0.2 --root /tmp/lua-rs-verify --force
/tmp/lua-rs-verify/bin/lua-rs -v
/tmp/lua-rs-verify/bin/lua-rs -e 'print(("ok"):upper())'
```

Then confirm the README/landing-page install line (`cargo install lua-cli`)
resolves to 0.0.2 and the documented CLI (REPL, `-e`, stdin `-`, `-v`) matches.
