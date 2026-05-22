# Agent Notes

This repo is an agent-driven Lua 5.4 to Rust port. Prefer narrow, test-led
changes over broad rewrites. When a test fails, use the debugging loop below.

## Debugging Playbook

1. Start from the newest concrete failure, not stale notes.
   - Inspect `harness/impl/official/<test>.out`.
   - Open the combined source around the reported line:
     `nl -ba harness/impl/official/<test>.combined.lua | sed -n 'A,Bp'`.
   - If the harness says `unknown`, check the process behavior and tail output;
     the wrapper can be wrong when its success markers change.

2. Turn the failure into a tiny repro before editing runtime code.
   - Use `target/debug/lua-rs -e '...'` for small Lua snippets.
   - Use a temporary copy under `/tmp` when you need to instrument an official
     test. Do not leave instrumentation in `reference/` or `harness/impl/official`.
   - Print the actual `(expected, actual)` pair for message-matching tests.

3. Patch the smallest runtime cause.
   - Fix the function that returned the wrong value or message.
   - Do not normalize an entire subsystem to satisfy one assertion.
   - Do not edit official tests except for temporary local diagnosis.
   - Treat "the failure moved deeper" as useful progress, but keep following it
     until the requested gate passes or the next blocker is architectural.

4. Use adjacent gates, not only the target test.
   - Error-message fixes should rerun `errors.lua` plus the test that exposed
     the mismatch.
   - Table/GC/coroutine changes should rerun `nextvar.lua`, `gc.lua` or the
     smallest relevant canary.
   - Harness changes should be checked through both `run_official_test.sh` and
     `run_one_test.sh`.

5. Watch for harness and environment bugs.
   - Official tests that inspect source names or line numbers must run from a
     real file path, not as one huge source-string argument.
   - Temp-file names must be unique across parallel `lua-rs` processes. Include
     process id plus time/counter if adding a generator.
   - Parallel official-test runs can collide through shared files if temp names
     are weak.

6. Keep temporary debugging out of the final diff.
   - Remove `print`, `eprintln!`, debug temp files, and one-off test edits.
   - Build before final verification: `cargo build -p lua-cli -q`.

## Useful Commands

```bash
cargo build -p lua-cli -q
./harness/run_official_test.sh reference/lua-c/testes/<test>.lua
./harness/run_one_test.sh reference/lua-c/testes/<test>.lua
tail -120 harness/impl/official/<test>.out
nl -ba harness/impl/official/<test>.combined.lua | sed -n 'START,ENDp'
```

For ghost/stub-sensitive changes:

```bash
./harness/check_ghost_abstractions.sh
```

For GC-sensitive changes:

```bash
./harness/canaries/gc/run_gc_canaries.sh
```

