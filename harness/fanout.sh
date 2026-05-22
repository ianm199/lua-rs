#!/usr/bin/env bash
# fanout.sh — per-file Phase A translation orchestrator.
#
# Usage:
#   ./harness/fanout.sh --pilot                 # the 5 pilot files (see below)
#   ./harness/fanout.sh --phase A               # all files in Phase A scope
#   ./harness/fanout.sh --files lctype.c lzio.c # an explicit list
#   ./harness/fanout.sh --workers 4 --pilot     # parallel; default is sequential
#
# Auth: this script DELIBERATELY uses the PERSONAL Claude Code account
# (`~/.claude-personal`) by exporting CLAUDE_CONFIG_DIR. It also unsets
# ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN so `claude -p` falls back to
# the subscription credentials in that config dir, not API credits.
# The preflight aborts if either env var is still set after unsetting.
#
# Output:
#   harness/oracle/results/pilot.jsonl — one JSON line per file with
#     {file, target_rust, status, cost_usd, duration_s, hooks_pass, notes}
#   harness/oracle/results/<basename>.translator.json — raw claude -p output
#   harness/oracle/results/<basename>.hooks.log       — hook output

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ──────────────────────────────────────────────────────────────────────────
# Preflight: subscription auth, tools available, clean workspace
# ──────────────────────────────────────────────────────────────────────────

# Route to the PERSONAL Claude Code account (NOT the work account).
# The `claude-personal` alias does this in interactive shells; scripts
# don't inherit aliases, so we set the underlying env var explicitly.
export CLAUDE_CONFIG_DIR="$HOME/.claude-personal"

if [ ! -d "$CLAUDE_CONFIG_DIR" ]; then
    echo "FATAL: $CLAUDE_CONFIG_DIR does not exist." >&2
    echo "       The personal Claude Code account isn't installed where expected." >&2
    exit 2
fi

unset ANTHROPIC_API_KEY
unset ANTHROPIC_AUTH_TOKEN

if [ -n "${ANTHROPIC_API_KEY:-}" ] || [ -n "${ANTHROPIC_AUTH_TOKEN:-}" ]; then
    echo "FATAL: ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN still set after unset." >&2
    echo "       Check your shell rc files — these would route claude -p to API billing." >&2
    exit 2
fi

for tool in claude jq awk; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "FATAL: '$tool' not found in PATH." >&2
        exit 2
    fi
done

if [ -n "$(git status --porcelain)" ]; then
    allow_dirty=0
    for a in "$@"; do [ "$a" = "--allow-dirty" ] && allow_dirty=1; done
    if [ "$allow_dirty" = "0" ]; then
        echo "WARN: working tree not clean. fanout writes commits per file." >&2
        echo "      Stash or commit before continuing, or pass --allow-dirty." >&2
        exit 2
    fi
fi

# ──────────────────────────────────────────────────────────────────────────
# Arg parsing
# ──────────────────────────────────────────────────────────────────────────

MODE=""
WORKERS=1
FILES=()
DRY_RUN=0
ALLOW_DIRTY=0

while [ $# -gt 0 ]; do
    case "$1" in
        --pilot)       MODE="pilot"; shift ;;
        --phase)       MODE="phase"; PHASE="$2"; shift 2 ;;
        --files)       MODE="files"; shift; while [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; do FILES+=("$1"); shift; done ;;
        --workers)     WORKERS="$2"; shift 2 ;;
        --dry-run)     DRY_RUN=1; shift ;;
        --allow-dirty) ALLOW_DIRTY=1; shift ;;
        -h|--help)
            sed -n '2,18p' "$0"
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

# Pilot file list — frozen here so it's reproducible across runs.
if [ "$MODE" = "pilot" ]; then
    FILES=(lctype.c lopcodes.c lzio.c lstring.c lmem.c)
fi

# Phase A file list — derived from ANALYSES/file_deps.txt.
# Per PORT_STRATEGY §4: Phase A scope = lexer + parser + bytecode emitter
# + the lua-vm support files (lobject, ltable, lstring, lstate, ldo, lvm,
# etc. — i.e. everything we need translated before we can compile per-crate
# in Phase B). lua-gc and lua-coro are deferred to Phases D/E.
if [ "$MODE" = "phase" ] && [ "$PHASE" = "A" ]; then
    while IFS=$'\t' read -r cfile crate _ ; do
        case "$crate" in
            lua-lex|lua-parse|lua-code|lua-vm) FILES+=("$cfile") ;;
        esac
    done < <(awk -F'[[:space:]]+' '!/^#/ && NF>=2 {print $1"\t"$2}' ANALYSES/file_deps.txt)
fi

if [ ${#FILES[@]} -eq 0 ]; then
    echo "no files to process (mode=$MODE)" >&2
    exit 2
fi

# ──────────────────────────────────────────────────────────────────────────
# Per-file translator invocation
# ──────────────────────────────────────────────────────────────────────────

RESULTS_DIR="harness/oracle/results"
mkdir -p "$RESULTS_DIR"
JSONL="$RESULTS_DIR/pilot.jsonl"
: > "$JSONL"

translate_one() {
    local cfile="$1"
    local basename="${cfile%.c}"
    local start_ts=$(date +%s)

    # Look up target crate + rust path
    local target_line
    target_line=$(awk -F'[[:space:]]+' -v c="$cfile" '!/^#/ && $1==c {print $2"\t"$3; exit}' ANALYSES/file_deps.txt)
    if [ -z "$target_line" ]; then
        echo "  [$cfile] SKIP: no mapping in ANALYSES/file_deps.txt" >&2
        echo "{\"file\":\"$cfile\",\"status\":\"no_mapping\"}" >> "$JSONL"
        return
    fi
    local crate=$(echo "$target_line" | awk '{print $1}')
    local rust_rel=$(echo "$target_line" | awk '{print $2}')
    local rust_full="crates/$crate/$rust_rel"

    # Idempotency: skip if rust file already exists and looks ported
    if [ -f "$rust_full" ] && grep -q "PORT STATUS" "$rust_full"; then
        echo "  [$cfile] SKIP: $rust_full already ported"
        echo "{\"file\":\"$cfile\",\"target\":\"$rust_full\",\"status\":\"already_ported\"}" >> "$JSONL"
        return
    fi

    echo "  [$cfile] → $rust_full"

    if [ "$DRY_RUN" = "1" ]; then
        echo "    (dry run; no claude -p invocation)"
        return
    fi

    local prompt="Translate the C file at \`reference/lua-5.4.7/src/$cfile\` to Rust at \`$rust_full\` per PORTING.md.

This is a Phase A task: faithful logic translation. The file does NOT need to compile.
Strict rules from PORTING.md:
- No String/&str/from_utf8 for Lua data — use &[u8], Vec<u8>, LuaString
- No unsafe outside lua-gc/lua-coro
- No tokio/async fn/futures/rayon
- Errors → Result<T, LuaError>; constructors from PORTING.md §6.1
- Stack refs → StackIdx, never borrows
- Flag don't guess: TODO(port) and stop when unsure
- Embed C source as // C: comments per HARNESS_DESIGN.md §10
- End with PORT STATUS trailer per PORTING.md §12

Use the Translator subagent (.claude/agents/translator.md). When done, stop — don't try to make it compile."

    local out_json="$RESULTS_DIR/$basename.translator.json"
    local hooks_log="$RESULTS_DIR/$basename.hooks.log"

    # Invocation. Notes:
    # - NO --bare: that flag refuses OAuth/keychain auth (API-key only), which
    #   would block us from using the subscription. See `claude --help`.
    # - --agent (singular) selects an autodiscovered subagent by name.
    # - --append-system-prompt takes the prompt as a string, not a file path.
    # - --settings and --agents-file equivalents are NOT needed because, without
    #   --bare, the CLI auto-discovers .claude/{settings.json,agents/} from cwd.
    # - --max-turns doesn't exist in this CLI version; --max-budget-usd is the
    #   effective bound on how long a single invocation can run.
    # - --output-format stream-json emits newline-delimited events as they
    #   happen, so we can show live progress. The full transcript goes to
    #   <basename>.transcript.jsonl; a one-line summary goes to <basename>
    #   .translator.json for status extraction.
    local porting_md transcript
    porting_md="$(cat PORTING.md)"
    transcript="$RESULTS_DIR/$basename.transcript.jsonl"

    claude -p \
        --agent translator \
        --append-system-prompt "$porting_md" \
        --allowedTools "Read,Write,Edit,Glob,Grep,Bash(cargo check*)" \
        --permission-mode dontAsk \
        --output-format stream-json \
        --include-partial-messages \
        --verbose \
        --max-budget-usd 2.00 \
        "$prompt" \
        2>>"$RESULTS_DIR/$basename.stderr" \
        | tee "$transcript" \
        | jq -rj --unbuffered '
            if .type == "system" and .subtype == "init" then
                "      [init] tools=\(.tools | length) model=\(.model // "?")\n"
            elif .type == "assistant" then
                ( .message.content // [] ) as $c |
                if ($c | length) > 0 and $c[0].type == "text" then
                    "      [text] \( ($c[0].text // "") | gsub("\n"; " ") | .[0:100] )...\n"
                elif ($c | length) > 0 and $c[0].type == "tool_use" then
                    "      [tool] \($c[0].name)(\( ($c[0].input | tostring) | .[0:80] ))\n"
                else empty
                end
            elif .type == "user" then
                ( .message.content // [] ) as $c |
                if ($c | length) > 0 and $c[0].type == "tool_result" then
                    "      [<-  ] \( ($c[0].content | tostring) | gsub("\n"; " ") | .[0:80] )\n"
                else empty
                end
            elif .type == "result" then
                "      [done] cost=$\(.total_cost_usd) turns=\(.num_turns) err=\(.is_error) reason=\(.stop_reason // "?")\n"
            else empty
            end
          ' 2>/dev/null >&2 || true

    # Build a single-blob JSON summary from the transcript's final "result" event
    jq -s 'map(select(.type == "result")) | .[-1] // {}' "$transcript" > "$out_json" 2>/dev/null || echo '{}' > "$out_json"

    # ── Syntax check ─────────────────────────────────────────────────────
    # Run rustc in isolation on the new file. Filter out name-resolution
    # errors (expected in Phase A — types like LuaState/LuaError are not yet
    # defined cross-crate) and rustc's own "aborting due to N previous
    # errors" summary line. Anything residual = real syntax issue.
    local syntax_log="$RESULTS_DIR/$basename.rustc.err"
    local syntax_ok="true"
    local syntax_residual=0
    if [ -f "$rust_full" ]; then
        # Use per-invocation tempfile so parallel workers don't race
        local rmeta_tmp
        rmeta_tmp=$(mktemp -t lua-rs-syntax.XXXXXX)
        rustc --edition 2021 --crate-type=lib --emit=metadata \
            -o "$rmeta_tmp" \
            "$rust_full" 2>"$syntax_log" >/dev/null || true
        local namefilt='cannot find|unresolved|no `[A-Z][a-zA-Z_]*`|use of undeclared|cannot find type|cannot find macro|cannot find function|cannot find value|cannot find trait|cannot find derive|associated function|associated item|cannot find attribute|aborting due to'
        syntax_residual=$(grep '^error' "$syntax_log" 2>/dev/null | grep -vE "$namefilt" | wc -l | tr -d ' ')
        if [ "$syntax_residual" -gt 0 ]; then
            syntax_ok="false"
        fi
        rm -f "$rmeta_tmp"
    fi

    local cost
    cost=$(jq -r '.total_cost_usd // 0' "$out_json" 2>/dev/null || echo 0)

    # Run the hooks against the new state
    {
        echo "=== unsafe-budget ==="
        bash .claude/hooks/unsafe-budget.sh; echo "exit=$?"
        echo "=== forbidden-import ==="
        bash .claude/hooks/forbidden-import.sh; echo "exit=$?"
        echo "=== trailer-required ==="
        bash .claude/hooks/trailer-required.sh; echo "exit=$?"
    } > "$hooks_log" 2>&1
    local hooks_pass="true"
    grep -q "exit=[^0]" "$hooks_log" && hooks_pass="false"

    local status="ok"
    [ ! -f "$rust_full" ] && status="no_output"
    [ "$hooks_pass" = "false" ] && status="hooks_failed"
    [ "$syntax_ok" = "false" ] && status="syntax_failed"

    local end_ts=$(date +%s)
    local duration=$((end_ts - start_ts))

    printf '{"file":"%s","target":"%s","status":"%s","cost_usd":%s,"duration_s":%d,"hooks_pass":%s,"syntax_ok":%s,"syntax_residual":%d}\n' \
        "$cfile" "$rust_full" "$status" "$cost" "$duration" "$hooks_pass" "$syntax_ok" "$syntax_residual" \
        >> "$JSONL"

    echo "    status=$status  cost=\$$cost  duration=${duration}s  hooks=$hooks_pass  syntax=$syntax_ok (residual=$syntax_residual)"
}

# ──────────────────────────────────────────────────────────────────────────
# Run
# ──────────────────────────────────────────────────────────────────────────

echo "fanout: mode=$MODE  files=${#FILES[@]}  workers=$WORKERS  dry_run=$DRY_RUN"
echo "         auth=personal subscription (CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR)"
echo "                                   (ANTHROPIC_API_KEY explicitly unset)"
echo

if [ "$WORKERS" = "1" ]; then
    for f in "${FILES[@]}"; do
        translate_one "$f"
    done
else
    # Naive xargs-based parallelism; for serious fanout, use git worktrees per Carlini.
    export -f translate_one
    export ROOT RESULTS_DIR JSONL DRY_RUN
    printf '%s\n' "${FILES[@]}" | xargs -n1 -P"$WORKERS" -I{} bash -c 'translate_one "$@"' _ {}
fi

echo
echo "─── SUMMARY ───"
total=$(wc -l < "$JSONL" | tr -d ' ')
ok=$(grep -c '"status":"ok"' "$JSONL" || true)
total_cost=$(jq -s 'map(.cost_usd // 0) | add' "$JSONL" 2>/dev/null || echo 0)
echo "  files processed: $total"
echo "  status=ok:       $ok"
echo "  total cost USD:  \$$total_cost  (note: subscription absorbs this; reported for tracking)"
echo
echo "Full results: $JSONL"
echo "Per-file outputs: $RESULTS_DIR/*.translator.json + *.hooks.log"
