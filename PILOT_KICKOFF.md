# Pilot Kickoff — how to fire the 5-file Phase A pilot

This is the runbook for the first real agent-driven translation pass. You'll run **five files** through the Translator subagent in a single command, validate the output via the hooks, and capture cost/duration/quality signal before scaling to the full Phase A.

## What gets translated

Five small, low-dependency C files chosen to exercise the harness without hitting the hard subsystems (no GC, no VM, no parser yet):

| File | LoC | Target crate | Why this one |
|---|---|---|---|
| `lctype.c` | ~50 | lua-vm | character-class tables; trivial, validates ANALYSES lookup |
| `lopcodes.c` | 104 | lua-code | the opmodes table; pairs nicely with the already-ported `opcode_names.rs` |
| `lzio.c` | ~75 | lua-vm | input-stream buffer; tests struct + method translation |
| `lstring.c` | 274 | lua-vm | string interning — first real `LuaString` work |
| `lmem.c` | 215 | lua-gc | memory allocator wrapper — first `lua-gc` crate touch (unsafe-budget=20) |

Total: ~720 C LoC. Expected Rust output: ~600 LoC. Expected total cost in token-equivalents: **$5–15 across all five files**.

## Before you fire it

### Step 1 — Verify you're on the PERSONAL subscription, not the work account or API credits

You have two Claude installations on this machine:

| Alias | Config dir | Used for |
|---|---|---|
| `claude` | `~/.claude` | Work account |
| `claude-personal` | `~/.claude-personal` | Personal account ← **this project** |

`claude-personal` is a shell alias (`CLAUDE_CONFIG_DIR=~/.claude-personal claude`). Shell aliases don't propagate to scripts, so `fanout.sh` sets `CLAUDE_CONFIG_DIR` itself.

```bash
# Verify the personal config dir exists and is authenticated:
ls -la ~/.claude-personal/

# Check personal account's auth status (interactive — Ctrl-C after you see the line):
claude-personal /status

# Check both API-key env vars are unset:
echo "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:-(unset — good)}"
echo "ANTHROPIC_AUTH_TOKEN=${ANTHROPIC_AUTH_TOKEN:-(unset — good)}"
```

If `ANTHROPIC_API_KEY` is set in your shell, find where (usually `~/.zshrc` or `~/.bashrc`) and unset it for this terminal:

```bash
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
```

The fanout script *also* unsets these and sets `CLAUDE_CONFIG_DIR=~/.claude-personal` defensively, but checking up front saves a surprise. As of May 2026 (today), `claude -p` on subscription draws from the same pool as interactive use. The June 15 split into a separate "Agent SDK credit" bucket is ~1 month out — plenty of runway.

### Step 2 — Confirm the workspace is clean

```bash
cd /Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port
git status --short      # should be empty
cargo check --workspace # should be green
```

If dirty, commit or stash. The fanout script writes one commit per file via the `commit-on-stop.sh` hook.

### Step 3 — Open a fresh terminal (NOT inside an interactive `claude` session)

The fanout script invokes `claude -p` as subprocesses. Running it from a plain shell keeps the cost accounting clean and avoids any weird nested-session behavior.

## Fire it

### Dry run first (no API calls, just verifies wiring)

```bash
cd /Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port
./harness/fanout.sh --pilot --dry-run
```

You should see output like:

```
fanout: mode=pilot  files=5  workers=1  dry_run=1
         auth=subscription (ANTHROPIC_API_KEY explicitly unset)

  [lctype.c] → crates/lua-vm/src/ctype.rs
    (dry run; no claude -p invocation)
  [lopcodes.c] → crates/lua-code/src/opcodes.rs
    (dry run; no claude -p invocation)
  ...
```

If any file says "SKIP: no mapping," there's a path mismatch in `ANALYSES/file_deps.txt` to fix before the real run.

### Real run

```bash
./harness/fanout.sh --pilot
```

What to expect:

- **Duration:** sequential, ~5–10 min per file = **30–50 minutes total**. The script prints per-file status as it goes.
- **Cost:** $1–3 per file (cap is $2; Translator usually comes in under). Total **$5–15** in token-equivalents. Subscription absorbs this.
- **Per-file output:** a `.rs` file in the target crate, ending with a PORT STATUS trailer. The script then runs all three Stop hooks against the new state and logs results.
- **Final summary:** prints to stdout; full results in `harness/oracle/results/pilot.jsonl`.

### To run faster (4-way parallel)

Only after the sequential pilot works once. Parallelism amplifies any flakiness.

```bash
./harness/fanout.sh --pilot --workers 4
```

## What success looks like

After the pilot completes you should see:

```
─── SUMMARY ───
  files processed: 5
  status=ok:       5
  total cost USD:  $XX.XX  (note: subscription absorbs this; reported for tracking)
```

And in the tree:

```
crates/lua-vm/src/ctype.rs           # was lctype.c
crates/lua-code/src/opcodes.rs       # was lopcodes.c
crates/lua-vm/src/zio.rs             # was lzio.c
crates/lua-vm/src/string.rs          # was lstring.c
crates/lua-gc/src/mem.rs             # was lmem.c
```

Each ends with a `PORT STATUS` trailer with `confidence: high|medium|low`. Files with `confidence: low` are flagged for human review in Phase B.

## If something goes wrong

### Hook failure on one file

`status=hooks_failed` in the JSONL. Read `harness/oracle/results/<file>.hooks.log` to see which hook failed and why. Common culprits:

- **`forbidden-import` failed:** the Translator slipped a `String`/`&str` past for Lua data. Re-prompt with a stricter reminder, or fix the file manually.
- **`trailer-required` failed:** the Translator forgot the PORT STATUS block. Add it, then `git commit`.
- **`unsafe-budget` failed:** the Translator wrote `unsafe` in a non-`lua-gc`/`lua-coro` crate. Almost always a real bug — replace with `TODO(port): unsafe needed for <reason>` and address in Phase B.

### `claude -p` exit non-zero / no output file produced

Check `harness/oracle/results/<file>.translator.json` for the error payload. Common:

- **Auth failure:** the preflight should have caught this, but if not, re-check `claude-personal /status`. Also verify `CLAUDE_CONFIG_DIR` is pointing at `~/.claude-personal` inside the script's environment (the script exports it; double-check it isn't being overridden by a parent shell).
- **`max-budget-usd` exceeded:** raise the cap to $3.00 in `fanout.sh` for the affected file and retry.
- **`max-turns` exceeded:** the file is more complex than expected. Bump `--max-turns` to 20 and retry, or split the file into smaller per-function tasks.

### Kill the run

Ctrl-C in the terminal stops the script cleanly between files. Mid-file `claude -p` invocations will continue until they finish naturally (a few minutes). To force-kill, `pkill -f 'claude -p'` from another terminal.

## After the pilot

Two questions to answer from the data:

1. **Is the cost per file within the $2–7 expected range?** If higher, look at the per-invocation JSON `usage` field to see whether token counts or turn counts are out of line.
2. **What's the `confidence` distribution?** If most files are `high`, scale up. If many are `low`, fix the PORTING.md spec or ANALYSES content first.

Once both look good: scale to all of Phase A via `./harness/fanout.sh --phase A`. Expect 25 more files, ~$150–400 in token-equivalents, ~2–4 days of agent time.

## Single-command summary

```bash
cd /Users/ianmclaughlin/PycharmProjects/rustExperiments/lua-rs-port \
  && unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN \
  && export CLAUDE_CONFIG_DIR="$HOME/.claude-personal" \
  && ls "$CLAUDE_CONFIG_DIR" >/dev/null \
  && ./harness/fanout.sh --pilot --dry-run \
  && echo "Dry run OK. Hit enter to fire the real pilot against personal account." \
  && read -r \
  && ./harness/fanout.sh --pilot
```
