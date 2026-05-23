#!/usr/bin/env bash
# harness/loop/status.sh — one-screen visibility into what the overnight
# loop is actually doing. Reads loop-state.json + the active transcript
# + recent commits + the bench history, prints a compact summary.
#
# Usage:
#   bash harness/loop/status.sh             # one-shot snapshot
#   bash harness/loop/status.sh --watch     # refresh every 15 seconds

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

WATCH=0
[ "${1:-}" = "--watch" ] && WATCH=1

snapshot() {
    clear 2>/dev/null || true
    echo "═══════════════════════════════════════════════════════════════════════"
    echo "  lua-rs-port loop status   ($(date '+%Y-%m-%d %H:%M:%S'))"
    echo "═══════════════════════════════════════════════════════════════════════"
    echo ""

    LOOP_PID=$(pgrep -f 'run-loop.py.*--project \.' | head -1)
    if [ -n "$LOOP_PID" ]; then
        LOOP_START=$(ps -p "$LOOP_PID" -o lstart= 2>/dev/null | sed 's/^ *//')
        printf "  LOOP        : RUNNING  PID=%s  started=%s\n" "$LOOP_PID" "$LOOP_START"
    else
        printf "  LOOP        : NOT RUNNING\n"
    fi

    STATE_FILE="$ROOT/harness/loop/state/loop-state.json"
    if [ -f "$STATE_FILE" ]; then
        python3 - "$STATE_FILE" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    s = json.load(f)
print(f"  ITERATION   : {s.get('iteration', 0)}  consecutive_failures={s.get('consecutive_failures', 0)}  same_packet_failures={s.get('same_packet_failures', 0)}")
print(f"  LAST OK     : {s.get('last_successful_packet') or '(none yet)'}")
print(f"  LAST FAIL   : {s.get('last_failed_packet') or '(none)'}")
hist = s.get('history', [])
if hist:
    print(f"  HISTORY     : {len(hist)} attempts:")
    for h in hist[-5:]:
        rc_d = h.get('dispatch_rc')
        rc_r = h.get('record_rc')
        status = "OK" if rc_d == 0 and rc_r == 0 else f"FAIL(d={rc_d},r={rc_r})"
        print(f"                  it{h.get('iteration')} {h.get('packet')} -> {status}")
PY
    else
        echo "  ITERATION   : (no state file yet)"
    fi
    echo ""

    PKT_FILE="$ROOT/harness/loop/state/next-packet.json"
    if [ -f "$PKT_FILE" ]; then
        CURRENT_PACKET=$(python3 -c "import json; print(json.load(open('$PKT_FILE')).get('packet_id', '?'))" 2>/dev/null)
        printf "  PACKET      : %s\n" "$CURRENT_PACKET"
    fi

    LATEST_T=$(ls -t "$ROOT/harness/loop/state/transcripts/"*.jsonl 2>/dev/null | head -1)
    if [ -n "$LATEST_T" ]; then
        T_BASENAME=$(basename "$LATEST_T")
        T_LINES=$(wc -l < "$LATEST_T" | tr -d ' ')
        T_SIZE=$(du -h "$LATEST_T" | awk '{print $1}')
        T_MTIME=$(stat -f "%Sm" -t "%H:%M:%S" "$LATEST_T" 2>/dev/null || stat -c "%y" "$LATEST_T" 2>/dev/null | cut -d' ' -f2 | cut -d. -f1)
        printf "  TRANSCRIPT  : %s  (%s lines, %s, last write %s)\n" "$T_BASENAME" "$T_LINES" "$T_SIZE" "$T_MTIME"
        echo ""
        echo "  -- Last 5 agent tool calls --------------------------------------------"
        python3 - "$LATEST_T" <<'PY'
import json, sys
events = []
with open(sys.argv[1]) as f:
    for line in f:
        try: events.append(json.loads(line))
        except: pass
calls = []
for ev in events:
    msg = ev.get("message", {})
    if isinstance(msg, dict):
        for c in msg.get("content", []) or []:
            if isinstance(c, dict) and c.get("type") == "tool_use":
                calls.append((c.get("name"), c.get("input", {}) or {}))
for name, inp in calls[-5:]:
    if name == "Bash":
        cmd = inp.get("command", "")
        print(f"    Bash: {(cmd[:90]+'...') if len(cmd) > 90 else cmd}")
    elif name in ("Read","Write","Edit"):
        print(f"    {name}: {inp.get('file_path', '?')}")
    elif name == "Grep":
        print(f"    Grep: pattern={inp.get('pattern','?')[:50]}")
    else:
        print(f"    {name}: ...")
PY
    fi
    echo ""

    echo "  -- Recent commits (newest first) --------------------------------------"
    git -C "$ROOT" log --oneline -8 | sed 's/^/    /'
    echo ""

    LEDGER="$ROOT/harness/evidence/ledger.jsonl"
    if [ -f "$LEDGER" ]; then
        echo "  -- Latest bench ratios per workload (from ledger) ---------------------"
        python3 - "$LEDGER" <<'PY'
import json, sys
latest = {}
with open(sys.argv[1]) as f:
    for line in f:
        try: row = json.loads(line)
        except: continue
        if row.get("kind") != "bench" or row.get("metric") != "wall_ratio":
            continue
        ts = row.get("ts", "")
        w = row.get("workload", "?")
        if w not in latest or latest[w].get("ts","") < ts:
            latest[w] = row
for w in sorted(latest):
    r = latest[w]
    flag = "OK" if r.get("value", 99) <= 1.5 else "  "
    print(f"    {flag}  {w:18s} {r.get('value', '?'):>6.2f}x   (commit {r.get('commit', '?')[:8]})")
PY
    fi
    echo "═══════════════════════════════════════════════════════════════════════"
}

if [ "$WATCH" = "1" ]; then
    while true; do
        snapshot
        echo ""
        echo "  refreshing in 15s, Ctrl-C to stop..."
        sleep 15
    done
else
    snapshot
fi
