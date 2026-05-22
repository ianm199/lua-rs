# How the loops work

Three diagrams, simplest to most detailed.

## 1. Outer loop — fanout.sh orchestrates across files

```
   ANALYSES/file_deps.txt              pilot.jsonl
   ─────────────────────               ──────────────
    llex.c    lua-lex   ...             {file: lstring.c, status: ok,    cost: 1.31, ...}
    lparser.c lua-parse ...             {file: ltype.c,   status: ok,    cost: 0.82, ...}
    lvm.c     lua-vm    ...             {file: lvm.c,     status: no_output, cost: 2.45, ...}
       │                                    ▲
       │ (file queue)                       │ (per-file row appended)
       ▼                                    │
  ┌─────────────────────────────────────────────────────────────────────┐
  │                     fanout.sh  (the orchestrator)                    │
  │                                                                      │
  │  for each cfile:                                                     │
  │      if target.rs already real-ported → SKIP                         │
  │      else dispatch to a worker via xargs -P N                        │
  │                                                                      │
  │     ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐               │
  │     │worker 1 │  │worker 2 │  │worker 3 │  │worker 4 │  (parallel)    │
  │     └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘               │
  │          │            │            │            │                     │
  │          ▼            ▼            ▼            ▼                     │
  │      translate_one()  on its assigned cfile (see Diagram 2)          │
  │          │            │            │            │                     │
  │          └────────────┴─────┬──────┴────────────┘                     │
  │                             ▼                                         │
  │                  append row to pilot.jsonl                            │
  └─────────────────────────────────────────────────────────────────────┘
```

One worker per CPU is typical (--workers 4). Workers don't talk to each other — coordination is via:
- **Filesystem** (each writes a different .rs target)
- **JSONL append** (each appends one row per file done)
- **Idempotency** on next run (skip files whose target.rs already has a real port trailer)

## 2. Inner loop — what one worker does per file

```
     translate_one(cfile, target_rust):
       │
       ▼
  ┌────────────────────────────────────────────────────────────────────────┐
  │                                                                         │
  │                       ╔════════════════════════╗                        │
  │                       ║   claude -p invocation ║   ← the AI agent loop  │
  │                       ╚════════════════════════╝                        │
  │                                                                         │
  │   inputs:                                                                │
  │     • system prompt = translator.md (the role) + PORTING.md (the spec)  │
  │     • user prompt   = "translate <cfile> → <target_rust> per the rules" │
  │     • tools allowed = Read, Write, Edit, Glob, Grep, rustc, cargo check │
  │     • cap           = $2.00 budget, dontAsk perms                       │
  │                                                                         │
  │   the agent runs turn-by-turn:                                          │
  │                                                                         │
  │       ┌──────────────────────────────────────────────────┐              │
  │       │  1. Read the C file                              │              │
  │       │  2. Read ANALYSES/{macros,types,error_sites}.tsv │              │
  │       │  3. Write the Rust file                          │              │
  │       │                                                  │              │
  │       │  4. ★ Run rustc — self syntax check ★            │  ◄── INSIDE-  │
  │       │                                                  │      LOOP    │
  │       │     errors are EITHER:                           │      VALIDATION│
  │       │       (a) name-resolution → ignore (Phase B)     │              │
  │       │       (b) real syntax     → loop back to step 3  │              │
  │       │                                                  │              │
  │       │  5. Write the PORT STATUS trailer                │              │
  │       │  6. STOP                                         │              │
  │       └──────────────────────────────────────────────────┘              │
  │                                                                         │
  │   exit signals:                                                         │
  │     • turn cap reached    → end_turn                                    │
  │     • budget cap reached  → exceeded_budget  (no output)                │
  │     • agent says stop     → end_turn                                    │
  │                                                                         │
  └────────────────────────────────────────────────────────────────────────┘
       │
       ▼
  ┌────────────────────────────────────────────────────────────────────────┐
  │   POST-AGENT validation  (external — fanout.sh runs these)              │
  │                                                                         │
  │     1. unsafe-budget.sh    — count unsafe blocks vs ceiling             │
  │     2. forbidden-import.sh — grep for banned patterns                   │
  │     3. trailer-required.sh — PORT STATUS trailer present                │
  │     4. rustc backstop      — same check the agent should have run       │
  │                                                                         │
  │   These pass → status = ok                                              │
  │   These fail → status = hooks_failed | syntax_failed | no_output         │
  └────────────────────────────────────────────────────────────────────────┘
       │
       ▼
   append pilot.jsonl row.  Worker picks up next file.
```

## 3. Where every check fires — defense in depth

```
                      INSIDE the agent's loop          OUTSIDE (fanout/hook)
                      ──────────────────────────       ──────────────────────

  forbidden imports     PORTING.md says "don't"  →     forbidden-import.sh hook
                                                       (Stop event)

  unsafe budget         PORTING.md says "ceiling 0"→   unsafe-budget.sh hook
                        outside lua-gc/lua-coro        (Stop event)

  PORT STATUS trailer   PORTING.md §12 mandates  →     trailer-required.sh hook
                                                       (Stop event)

  syntax errors         translator.md mandates    →    rustc backstop in fanout
                        running rustc + iterating      (post-agent)
                        ★ this is the new addition

  budget overrun        (agent can't fix this)         --max-budget-usd cap
                                                       fanout records no_output

  test pass / fail      (not in Phase A; Phase C+)     diff-output.sh oracle
                                                       (later phases)
```

The mental model: **anything the agent can fix should be inside its loop.** External checks are the safety net for the things the agent can't fix (budget) or might skip (regression-prone validation).

This is why the syntax-check move mattered so much: previously the agent declared "done" without ever seeing rustc's complaints. Three files (ltm, lobject, ldo) shipped broken syntax because the validation was post-hoc, not in-loop.

## What's still missing (future improvements)

- **`SubagentStop` hook with `{decision: "block"}`** — structural enforcement: if a post-hoc check fails, force the agent to keep going. The current setup just records the failure; we have to re-run manually. A Stop hook that *blocks* the exit and re-injects the failure into the agent's context is the next step up in discipline.
- **Self-check for forbidden imports + unsafe budget**, same shape as the syntax check.
- **Inter-worker coordination on shared types** (lua-types). Right now if two workers both reference `LuaError` and one defines it slightly differently, we won't notice until Phase B. A shared lock or "the first worker to touch lua-types defines the canonical version" rule would prevent drift.
