#!/usr/bin/env python3
"""Phase A monitor — a small curses TUI over the Backend protocol.

Usage:
    ./harness/monitor/monitor.py            # live: reads from harness/oracle/results/
    ./harness/monitor/monitor.py --mock     # synthetic data for UI development

Keys:
    q       quit
    r       force refresh
    j / k   select next / previous file (currently informational only)
    e       toggle expanded last-event view
"""

from __future__ import annotations

import argparse
import curses
import time
from pathlib import Path

from backend import Backend, FileEntry, LiveBackend, MockBackend, Summary


# ──────────────────────────────────────────────────────────────────────────
# Status glyphs and colors
# ──────────────────────────────────────────────────────────────────────────

GLYPH = {
    "wait": "·",
    "work": "▶",
    "done": "✓",
    "fail": "✗",
    "skip": "−",
}

COLOR_PAIR = {
    "wait": 1,
    "work": 2,
    "done": 3,
    "fail": 4,
    "skip": 5,
}


def init_colors() -> None:
    curses.start_color()
    curses.use_default_colors()
    curses.init_pair(1, curses.COLOR_WHITE,  -1)  # wait
    curses.init_pair(2, curses.COLOR_YELLOW, -1)  # work
    curses.init_pair(3, curses.COLOR_GREEN,  -1)  # done
    curses.init_pair(4, curses.COLOR_RED,    -1)  # fail
    curses.init_pair(5, curses.COLOR_CYAN,   -1)  # skip
    curses.init_pair(6, curses.COLOR_BLUE,   -1)  # header
    curses.init_pair(7, curses.COLOR_WHITE,  -1)  # dim text


# ──────────────────────────────────────────────────────────────────────────
# Formatters
# ──────────────────────────────────────────────────────────────────────────

def fmt_duration(s: int | None) -> str:
    if s is None or s < 0:
        return " — "
    m, sec = divmod(int(s), 60)
    return f"{m:>3}:{sec:02d}"


def fmt_cost(c: float | None) -> str:
    if c is None:
        return "    — "
    return f"${c:>5.2f}"


def truncate(s: str, n: int) -> str:
    if len(s) <= n:
        return s
    if n <= 1:
        return s[:n]
    return s[: n - 1] + "…"


# ──────────────────────────────────────────────────────────────────────────
# Rendering
# ──────────────────────────────────────────────────────────────────────────

def render(stdscr, files: list[FileEntry], summary: Summary,
           expanded: bool, refresh_pulse: bool, backend) -> None:
    stdscr.erase()
    max_y, max_x = stdscr.getmaxyx()

    # ─ Header ──────────────────────────────────────────────────────────
    pulse = "↻ " if refresh_pulse else "  "
    title = f" Phase A Monitor — lua-rs-port{pulse if refresh_pulse else ''}"
    clock = time.strftime("%H:%M:%S", time.localtime())
    if refresh_pulse:
        title = f" {pulse}Phase A Monitor — lua-rs-port (refreshed)"
    stdscr.addnstr(0, 0, title.ljust(max_x - len(clock) - 1) + clock,
                   max_x, curses.color_pair(6) | curses.A_BOLD)

    elapsed = fmt_duration(summary.elapsed_s).strip()
    summary_line = (
        f" {summary.done_count} done · {summary.fail_count} fail · "
        f"{summary.work_count} work · {summary.wait_count} wait · "
        f"{summary.skip_count} skip   |   "
        f"elapsed {elapsed}   |   spent ${summary.total_cost:.2f}"
    )
    stdscr.addnstr(1, 0, summary_line[:max_x], max_x, curses.A_DIM)

    # ─ Column headers ─────────────────────────────────────────────────
    header = " ST  FILE              TARGET                            COST    DUR    HK SX"
    stdscr.addnstr(3, 0, header[:max_x], max_x, curses.A_BOLD)
    stdscr.addnstr(4, 0, ("─" * max_x)[:max_x], max_x, curses.A_DIM)

    # ─ Rows ───────────────────────────────────────────────────────────
    row = 5
    for f in files:
        if row >= max_y - 2:
            break
        glyph = GLYPH.get(f.status, "?")
        cp = curses.color_pair(COLOR_PAIR.get(f.status, 1))
        target_short = f.target.replace("crates/", "").replace("/src/", "/")
        hk = "  " if f.hooks_pass is None else ("✓ " if f.hooks_pass else "✗ ")
        sx = "  " if f.syntax_ok is None else ("✓ " if f.syntax_ok else "✗ ")
        line = (
            f" {glyph}  "
            f"{truncate(f.cfile, 17):<17} "
            f"{truncate(target_short, 33):<33} "
            f"{fmt_cost(f.cost_usd):>7} "
            f"{fmt_duration(f.duration_s):>6}  "
            f"{hk}{sx}"
        )
        stdscr.addnstr(row, 0, line[:max_x], max_x, cp)
        row += 1

        if f.status == "work" and row < max_y - 2:
            if expanded:
                for ev in backend.events(f.cfile, limit=5):
                    if row >= max_y - 2:
                        break
                    line = f"       ⤷ [{ev.type}] {ev.summary[:max_x - 16]}"
                    stdscr.addnstr(row, 0, line[:max_x], max_x,
                                   curses.color_pair(7) | curses.A_DIM)
                    row += 1
            elif f.last_event:
                event_line = "       ⤷ " + f.last_event
                stdscr.addnstr(row, 0, event_line[:max_x], max_x,
                               curses.color_pair(7) | curses.A_DIM)
                row += 1
        elif f.status == "fail" and f.last_event and row < max_y - 2:
            event_line = "       ⤷ " + f.last_event
            stdscr.addnstr(row, 0, event_line[:max_x], max_x,
                           curses.color_pair(4) | curses.A_DIM)
            row += 1

    # ─ Footer ─────────────────────────────────────────────────────────
    mode_tag = " [expanded]" if expanded else ""
    footer = f" q quit · r refresh now · e toggle expanded{mode_tag} · auto 1s"
    stdscr.addnstr(max_y - 1, 0, footer[:max_x], max_x, curses.A_REVERSE)

    stdscr.noutrefresh()
    curses.doupdate()


# ──────────────────────────────────────────────────────────────────────────
# Main loop
# ──────────────────────────────────────────────────────────────────────────

def loop(stdscr, backend: Backend, refresh_s: float) -> None:
    curses.curs_set(0)
    stdscr.nodelay(True)
    stdscr.timeout(int(refresh_s * 1000))
    init_colors()

    expanded = False
    refresh_pulse_until = 0.0
    while True:
        files = backend.files()
        summary = backend.summary()
        pulse = time.time() < refresh_pulse_until
        render(stdscr, files, summary, expanded, pulse, backend)
        try:
            key = stdscr.getch()
        except KeyboardInterrupt:
            return
        if key in (ord("q"), 27):
            return
        if key == ord("r"):
            refresh_pulse_until = time.time() + 0.8
            continue
        if key == ord("e"):
            expanded = not expanded


def main() -> None:
    parser = argparse.ArgumentParser(description="Phase A monitor TUI.")
    parser.add_argument("--mock", action="store_true",
                        help="Use synthetic data; no harness/oracle/results/ required.")
    parser.add_argument("--root", type=Path,
                        default=Path(__file__).resolve().parents[2],
                        help="Project root (default: this script's grandparent).")
    parser.add_argument("--refresh", type=float, default=1.0,
                        help="Refresh interval in seconds (default 1.0).")
    args = parser.parse_args()

    backend: Backend = MockBackend() if args.mock else LiveBackend(args.root)
    curses.wrapper(loop, backend, args.refresh)


if __name__ == "__main__":
    main()
