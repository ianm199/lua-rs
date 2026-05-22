"""Data backend for the Phase A monitor TUI.

Two implementations:
    - LiveBackend: reads from harness/oracle/results/*.transcript.jsonl and
      pilot.jsonl + ANALYSES/file_deps.txt to reconstruct the current state.
    - MockBackend: returns hand-rolled synthetic data, with optional
      simulated progress, so the TUI can be developed and tested without
      a running fanout.

The TUI never imports from anything else; swap backends via the CLI.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol


# ──────────────────────────────────────────────────────────────────────────
# Data types
# ──────────────────────────────────────────────────────────────────────────

FileStatus = str  # one of: "wait" "work" "done" "fail" "skip"


@dataclass
class FileEntry:
    cfile: str                     # "lvm.c"
    target: str                    # "crates/lua-vm/src/vm.rs"
    status: FileStatus
    cost_usd: float | None = None
    duration_s: int | None = None
    syntax_ok: bool | None = None
    hooks_pass: bool | None = None
    last_event: str | None = None  # one-line summary of the latest tool/text event
    started_at: float | None = None


@dataclass
class Event:
    type: str    # "init" | "text" | "tool" | "tool_result" | "done"
    summary: str # one-line excerpt


@dataclass
class Summary:
    started_at: float
    elapsed_s: int
    total_cost: float
    total_files: int
    done_count: int
    fail_count: int
    work_count: int
    wait_count: int
    skip_count: int


class Backend(Protocol):
    def files(self) -> list[FileEntry]: ...
    def events(self, cfile: str, limit: int = 5) -> list[Event]: ...
    def summary(self) -> Summary: ...


# ──────────────────────────────────────────────────────────────────────────
# Live backend
# ──────────────────────────────────────────────────────────────────────────

class LiveBackend:
    """Reads state from disk on every call. Cheap enough to poll every 500ms."""

    def __init__(self, project_root: Path) -> None:
        self.root = project_root
        self.results_dir = project_root / "harness" / "oracle" / "results"
        self.jsonl_path = self.results_dir / "pilot.jsonl"
        self.deps_path = project_root / "ANALYSES" / "file_deps.txt"
        self._started_at: float = self._infer_start_time()

    def _infer_start_time(self) -> float:
        if self.jsonl_path.exists():
            return self.jsonl_path.stat().st_mtime
        return time.time()

    def _queue_from_deps(self) -> list[tuple[str, str]]:
        """Return [(cfile, target_rust_path), ...] in declaration order."""
        out: list[tuple[str, str]] = []
        if not self.deps_path.exists():
            return out
        for line in self.deps_path.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) < 3:
                continue
            cfile, crate, rust_rel = parts[0], parts[1], parts[2]
            if crate in {"(none)"}:
                continue
            target = f"crates/{crate}/{rust_rel}"
            out.append((cfile, target))
        return out

    def _completed_records(self) -> dict[str, dict]:
        """Map cfile -> the latest record for it in pilot.jsonl."""
        out: dict[str, dict] = {}
        if not self.jsonl_path.exists():
            return out
        for line in self.jsonl_path.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            cfile = rec.get("file")
            if cfile:
                out[cfile] = rec
        return out

    def _last_event_summary(self, cfile: str) -> str | None:
        """Tail the transcript file and return a one-line excerpt of the latest event."""
        basename = cfile.rsplit(".", 1)[0]
        path = self.results_dir / f"{basename}.transcript.jsonl"
        if not path.exists():
            return None
        # Read last ~50KB only to avoid loading huge transcripts
        try:
            with path.open("rb") as f:
                f.seek(0, 2)
                size = f.tell()
                f.seek(max(0, size - 50_000))
                tail = f.read().decode("utf-8", errors="replace")
        except OSError:
            return None
        latest: str | None = None
        for raw in tail.splitlines():
            raw = raw.strip()
            if not raw:
                continue
            try:
                ev = json.loads(raw)
            except json.JSONDecodeError:
                continue
            t = ev.get("type")
            if t == "assistant":
                content = ((ev.get("message") or {}).get("content") or [])
                if content:
                    c0 = content[0]
                    ctype = c0.get("type")
                    if ctype == "text":
                        text = (c0.get("text") or "").replace("\n", " ").strip()
                        if text:
                            latest = f"text: {text[:90]}"
                    elif ctype == "tool_use":
                        name = c0.get("name", "?")
                        inp = json.dumps(c0.get("input") or {}, ensure_ascii=False)
                        latest = f"tool: {name}({inp[:70]})"
            elif t == "user":
                content = ((ev.get("message") or {}).get("content") or [])
                if content:
                    c0 = content[0]
                    if c0.get("type") == "tool_result":
                        body = c0.get("content")
                        if isinstance(body, list) and body and isinstance(body[0], dict):
                            text = (body[0].get("text") or "").replace("\n", " ").strip()
                        elif isinstance(body, str):
                            text = body.replace("\n", " ").strip()
                        else:
                            text = ""
                        if text:
                            latest = f"  <-: {text[:80]}"
            elif t == "result":
                cost = ev.get("total_cost_usd", 0.0)
                turns = ev.get("num_turns", "?")
                latest = f"done: cost=${cost:.2f} turns={turns}"
        return latest

    def files(self) -> list[FileEntry]:
        queue = self._queue_from_deps()
        completed = self._completed_records()
        active_phase = {"lua-lex", "lua-parse", "lua-code", "lua-vm"}
        out: list[FileEntry] = []
        for cfile, target in queue:
            crate = target.split("/", 2)[1] if target.startswith("crates/") else ""
            if crate not in active_phase:
                continue
            target_path = self.root / target
            transcript = self.results_dir / f"{cfile.rsplit('.',1)[0]}.transcript.jsonl"
            rec = completed.get(cfile)
            entry = FileEntry(cfile=cfile, target=target, status="wait")
            if rec:
                status_raw = rec.get("status", "ok")
                if status_raw == "already_ported":
                    entry.status = "skip"
                elif status_raw in {"hooks_failed", "syntax_failed", "no_output", "no_mapping"}:
                    entry.status = "fail"
                else:
                    entry.status = "done"
                entry.cost_usd = rec.get("cost_usd")
                entry.duration_s = rec.get("duration_s")
                sx = rec.get("syntax_ok")
                entry.syntax_ok = None if sx is None else bool(sx)
                hp = rec.get("hooks_pass")
                entry.hooks_pass = None if hp is None else bool(hp)
            elif transcript.exists():
                entry.status = "work"
                entry.last_event = self._last_event_summary(cfile)
                entry.started_at = transcript.stat().st_mtime
            elif target_path.exists() and self._is_real_port(target_path):
                # Real port already on disk but no JSONL entry yet (e.g. pilot output)
                entry.status = "done"
            out.append(entry)
        return out

    @staticmethod
    def _is_real_port(path: Path) -> bool:
        """Detect a real port vs a skeleton trailer (mirrors fanout.sh logic)."""
        try:
            text = path.read_text(errors="replace")
        except OSError:
            return False
        has_c_source = any(
            line.lstrip().startswith("//") and "source:" in line and (".c" in line or ".h" in line)
            for line in text.splitlines()
        )
        skeleton = "source:" in text and "(none" in text and "skeleton" in text
        return has_c_source and not skeleton

    def events(self, cfile: str, limit: int = 5) -> list[Event]:
        basename = cfile.rsplit(".", 1)[0]
        path = self.results_dir / f"{basename}.transcript.jsonl"
        if not path.exists():
            return []
        events: list[Event] = []
        try:
            with path.open() as f:
                lines = f.readlines()
        except OSError:
            return []
        for raw in lines:
            raw = raw.strip()
            if not raw:
                continue
            try:
                ev = json.loads(raw)
            except json.JSONDecodeError:
                continue
            t = ev.get("type")
            if t == "assistant":
                content = ((ev.get("message") or {}).get("content") or [])
                if content:
                    c0 = content[0]
                    if c0.get("type") == "text":
                        text = (c0.get("text") or "").replace("\n", " ").strip()
                        if text:
                            events.append(Event("text", text[:200]))
                    elif c0.get("type") == "tool_use":
                        name = c0.get("name", "?")
                        inp = json.dumps(c0.get("input") or {}, ensure_ascii=False)[:120]
                        events.append(Event("tool", f"{name}({inp})"))
            elif t == "result":
                cost = ev.get("total_cost_usd", 0.0)
                turns = ev.get("num_turns", "?")
                events.append(Event("done", f"cost=${cost:.4f} turns={turns}"))
        return events[-limit:]

    def summary(self) -> Summary:
        files = self.files()
        total_cost = sum((f.cost_usd or 0.0) for f in files)
        done = sum(1 for f in files if f.status == "done")
        fail = sum(1 for f in files if f.status == "fail")
        work = sum(1 for f in files if f.status == "work")
        wait = sum(1 for f in files if f.status == "wait")
        skip = sum(1 for f in files if f.status == "skip")
        return Summary(
            started_at=self._started_at,
            elapsed_s=int(time.time() - self._started_at),
            total_cost=total_cost,
            total_files=len(files),
            done_count=done,
            fail_count=fail,
            work_count=work,
            wait_count=wait,
            skip_count=skip,
        )


# ──────────────────────────────────────────────────────────────────────────
# Mock backend — for TUI development
# ──────────────────────────────────────────────────────────────────────────

class MockBackend:
    """Synthetic data with simulated progress: a 'work' entry advances every few seconds."""

    def __init__(self) -> None:
        self._start = time.time()
        self._files: list[FileEntry] = [
            FileEntry("lopcodes.c", "crates/lua-code/src/opcodes.rs", "skip"),
            FileEntry("lctype.c", "crates/lua-vm/src/ctype.rs", "done",
                      cost_usd=0.82, duration_s=291, syntax_ok=True, hooks_pass=True),
            FileEntry("lzio.c", "crates/lua-vm/src/zio.rs", "done",
                      cost_usd=0.64, duration_s=352, syntax_ok=True, hooks_pass=True),
            FileEntry("lstring.c", "crates/lua-vm/src/string.rs", "done",
                      cost_usd=1.31, duration_s=777, syntax_ok=True, hooks_pass=True),
            FileEntry("lparser.c", "crates/lua-parse/src/lib.rs", "fail",
                      cost_usd=2.32, duration_s=1279, syntax_ok=None, hooks_pass=True,
                      last_event="hit --max-budget-usd cap"),
            FileEntry("lvm.c", "crates/lua-vm/src/vm.rs", "work",
                      cost_usd=1.45, duration_s=1100, started_at=time.time() - 1100),
            FileEntry("ldo.c", "crates/lua-vm/src/do_.rs", "work",
                      cost_usd=0.92, duration_s=721, started_at=time.time() - 721),
            FileEntry("lcode.c", "crates/lua-code/src/codegen.rs", "work",
                      cost_usd=1.30, duration_s=1091, started_at=time.time() - 1091),
            FileEntry("llex.c", "crates/lua-lex/src/lib.rs", "work",
                      cost_usd=0.65, duration_s=465, started_at=time.time() - 465),
            FileEntry("lstate.c", "crates/lua-vm/src/state.rs", "wait"),
            FileEntry("ltm.c", "crates/lua-vm/src/tagmethods.rs", "wait"),
            FileEntry("lobject.c", "crates/lua-vm/src/object.rs", "wait"),
            FileEntry("lapi.c", "crates/lua-vm/src/api.rs", "wait"),
            FileEntry("ldebug.c", "crates/lua-vm/src/debug.rs", "wait"),
            FileEntry("ldump.c", "crates/lua-vm/src/dump.rs", "wait"),
            FileEntry("lundump.c", "crates/lua-vm/src/undump.rs", "wait"),
            FileEntry("lfunc.c", "crates/lua-vm/src/func.rs", "wait"),
            FileEntry("ltable.c", "crates/lua-vm/src/table.rs", "wait"),
        ]
        self._mock_events: dict[str, list[str]] = {
            "lvm.c": [
                "text: Reading the C source and porting guide...",
                "tool: Read({\"file_path\":\"reference/lua-5.4.7/src/lvm.c\"})",
                "  <-: /* * $Id: lvm.c $ * Lua virtual machine ...",
                "tool: Read({\"file_path\":\"ANALYSES/macros.tsv\"})",
                "  <-: # macros.tsv — every public macro in Lua headers...",
                "text: Translating the main interpreter loop...",
                "tool: Write({\"file_path\":\"crates/lua-vm/src/vm.rs\"})",
            ],
            "ldo.c": [
                "text: Mapping luaD_pcall to state.protected_call(...).",
                "tool: Read({\"file_path\":\"ANALYSES/error_sites.tsv\"})",
                "tool: Edit({\"file_path\":\"crates/lua-vm/src/do_.rs\"})",
            ],
            "lcode.c": [
                "tool: Read({\"file_path\":\"reference/lua-5.4.7/src/lcode.c\"})",
                "text: Translating jump-list patching helpers...",
                "tool: Write({\"file_path\":\"crates/lua-code/src/codegen.rs\"})",
            ],
            "llex.c": [
                "tool: Read({\"file_path\":\"reference/lua-5.4.7/src/llex.c\"})",
                "text: Building the lexer state machine in Rust...",
            ],
        }
        self._tick = 0

    def files(self) -> list[FileEntry]:
        # Simulated progress: each call advances the work entries' last_event
        self._tick += 1
        out: list[FileEntry] = []
        for f in self._files:
            f2 = FileEntry(**f.__dict__)
            if f2.status == "work":
                events = self._mock_events.get(f2.cfile, [f"text: working on {f2.cfile}..."])
                f2.last_event = events[self._tick % len(events)]
                if f2.started_at:
                    f2.duration_s = int(time.time() - f2.started_at)
            out.append(f2)
        return out

    def events(self, cfile: str, limit: int = 5) -> list[Event]:
        evs = self._mock_events.get(cfile, [])
        return [Event(e.split(":", 1)[0].strip(), e.split(":", 1)[1].strip()) for e in evs[-limit:]]

    def summary(self) -> Summary:
        files = self.files()
        total_cost = sum((f.cost_usd or 0.0) for f in files)
        done = sum(1 for f in files if f.status == "done")
        fail = sum(1 for f in files if f.status == "fail")
        work = sum(1 for f in files if f.status == "work")
        wait = sum(1 for f in files if f.status == "wait")
        skip = sum(1 for f in files if f.status == "skip")
        return Summary(
            started_at=self._start,
            elapsed_s=int(time.time() - self._start),
            total_cost=total_cost,
            total_files=len(files),
            done_count=done,
            fail_count=fail,
            work_count=work,
            wait_count=wait,
            skip_count=skip,
        )
