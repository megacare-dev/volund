#!/usr/bin/env python3
"""Turns Völund's run logs into CSV tables for the writeup (DESIGN.md §9).

The harness logs one JSON line per event: `t` (seconds since the task arrived), `phase`, `kind`
(start, probe, transition, tool, llm, check, context, model_tests, rollback, verify_changed), and
that kind's fields. The evaluator keeps them in each trial's `agent/` folder of a run made by
`eval/run.py`:

    runs/<run>/attempt<N>/jobs/dev/<task>__<id>/agent/serve.log   (`volund: {...}`, all of them)
    runs/<run>/attempt<N>/jobs/dev/<task>__<id>/agent/<name>.txt  (`harness: {...}`, cut in the middle when long)

serve.log is read when it holds events, the .txt file otherwise. Scores come from the run's
results.json when it is there.

Writes three tables to the output folder:

    events.csv  one row per event
    phases.csv  one row per trial and phase: visits, seconds, tool and model calls, tokens
    tasks.csv   one row per trial: level, model, score, the phases it went through, the last
                transition's reason, FIX rounds, rollbacks, tokens

Usage:
    python3 tools/logs_to_csv.py runs/<run> [more runs or agent folders ...] [-o out/]

Standard library only.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import sys
from collections import OrderedDict
from pathlib import Path

PREFIXES = ("volund: ", "harness: ")
TOKEN_FIELDS = ("input_tokens", "output_tokens", "cache_read_tokens")
STRAY_CHECK = "no stray files in the task's folders"

EVENT_COLUMNS = [
    "run", "attempt", "task", "trial", "source", "seq", "t", "phase", "kind",
    "event", "to", "reason", "effects", "name", "args",
    *TOKEN_FIELDS, "failed_checks", "data",
]
PHASE_COLUMNS = [
    "run", "attempt", "task", "trial", "level", "phase", "visits", "seconds",
    "tool_calls", "llm_calls", *TOKEN_FIELDS,
]
TASK_COLUMNS = [
    "run", "attempt", "task", "trial", "level", "model", "score", "measured", "status",
    "seconds", "path", "last_reason", "fix_rounds", "rollbacks", "verify_changed",
    "stray_checks_failed", "tool_calls", "llm_calls", *TOKEN_FIELDS, "source",
]


def parse_line(line: str) -> dict | None:
    """The event a log line holds, or None for any other line."""
    text = line.strip()
    for prefix in PREFIXES:
        if text.startswith(prefix):
            text = text[len(prefix):]
            break
    if not text.startswith("{"):
        return None
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        return None
    if isinstance(value, dict) and "kind" in value and "phase" in value:
        return value
    return None


def read_events(path: Path) -> list[dict]:
    with path.open(encoding="utf-8", errors="replace") as f:
        return [e for e in map(parse_line, f) if e is not None]


def trial_events(agent: Path) -> tuple[str, list[dict]]:
    """The events of one trial's agent folder, and the file they came from."""
    serve = agent / "serve.log"
    if serve.is_file():
        events = read_events(serve)
        if events:
            return serve.name, events
    for txt in sorted(agent.glob("*.txt")):
        events = read_events(txt)
        if events:
            return txt.name, events
    return "", []


def where(agent: Path) -> dict:
    """Run, attempt, task and trial from the folder layout; empty where it does not match."""
    trial = agent.parent if agent.name == "agent" else agent
    attempt = next((p for p in trial.parents if re.fullmatch(r"attempt\d+", p.name)), None)
    run_dir = attempt.parent if attempt else None
    return {
        "run": run_dir.name if run_dir else "",
        "attempt": attempt.name if attempt else "",
        "task": trial.name.split("__")[0],
        "trial": trial.name,
        "_trial_dir": trial,
        "_run_dir": run_dir,
    }


def scores(run_dir: Path | None, cache: dict) -> dict:
    """The run's results.json rows by trial path relative to the run folder."""
    if run_dir is None:
        return {}
    if run_dir not in cache:
        rows = {}
        try:
            saved = json.loads((run_dir / "results.json").read_text(encoding="utf-8"))
            for row in saved.get("tasks", []):
                if row.get("trial"):
                    rows[os.path.normpath(row["trial"])] = row
        except (OSError, ValueError, AttributeError):
            pass
        cache[run_dir] = rows
    return cache[run_dir]


def cell(value) -> str:
    if value is None:
        return ""
    if isinstance(value, (dict, list)):
        return json.dumps(value, ensure_ascii=False, sort_keys=True)
    return str(value)


def event_row(key: dict, source: str, seq: int, e: dict) -> dict:
    row = {k: key[k] for k in ("run", "attempt", "task", "trial")}
    row.update(source=source, seq=seq)
    used = {"t", "phase", "kind", "event", "to", "reason", "effects", "name", "args", *TOKEN_FIELDS}
    for k in used:
        row[k] = cell(e.get(k))
    if e.get("kind") == "check" and isinstance(e.get("checks"), list):
        failed = [c.get("name", "") for c in e["checks"] if isinstance(c, dict) and not c.get("passed")]
        row["failed_checks"] = "; ".join(failed)
    rest = {k: v for k, v in e.items() if k not in used}
    row["data"] = cell(rest) if rest else ""
    return row


def number(value) -> float:
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) else 0


def summarise(events: list[dict]) -> tuple[OrderedDict, dict]:
    """Per-phase totals (in first-seen order) and per-trial totals of one trial's events."""
    phases: OrderedDict[str, dict] = OrderedDict()

    def phase(name: str) -> dict:
        return phases.setdefault(name, {"visits": 0, "seconds": 0.0, "tool_calls": 0, "llm_calls": 0,
                                        **{k: 0 for k in TOKEN_FIELDS}})

    trial = {"level": "", "model": "", "path": [], "last_reason": "", "fix_rounds": 0, "rollbacks": 0,
             "verify_changed": 0, "stray_checks_failed": 0, "seconds": 0.0}
    began = number(events[0].get("t")) if events else 0
    ended_in = ""
    for e in events:
        name, kind, t = str(e.get("phase", "")), e.get("kind"), number(e.get("t"))
        p = phase(name)
        trial["seconds"] = max(trial["seconds"], t)
        if kind == "start":
            trial["level"] = cell(e.get("level"))
            trial["model"] = cell(e.get("model"))
        elif kind == "tool":
            p["tool_calls"] += 1
        elif kind == "llm" and "error" not in e:
            p["llm_calls"] += 1
            for k in TOKEN_FIELDS:
                p[k] += int(number(e.get(k)))
        elif kind == "transition":
            p["visits"] += 1
            p["seconds"] += max(0.0, t - began)
            began = t
            trial["path"].append(name)
            trial["last_reason"] = cell(e.get("reason"))
            ended_in = cell(e.get("to"))
            if e.get("to") == "FIX":
                trial["fix_rounds"] += 1
            if "rollback" in (e.get("effects") or []):
                trial["rollbacks"] += 1
        elif kind == "verify_changed":
            trial["verify_changed"] += 1
        elif kind == "check" and isinstance(e.get("checks"), list):
            if any(isinstance(c, dict) and c.get("name") == STRAY_CHECK and not c.get("passed")
                   for c in e["checks"]):
                trial["stray_checks_failed"] += 1
    if events:
        # the phase the run ended in (FINALIZE, or the loop's one phase) has no transition out
        last = ended_in or str(events[-1].get("phase", ""))
        p = phase(last)
        p["visits"] += 1
        p["seconds"] += max(0.0, number(events[-1].get("t")) - began)
        trial["path"].append(last)
    return phases, trial


def write(path: Path, columns: list[str], rows: list[dict]) -> None:
    with path.open("w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=columns, extrasaction="ignore")
        w.writeheader()
        w.writerows(rows)


def agent_folders(paths: list[Path]) -> list[Path]:
    found = []
    for p in paths:
        if p.is_dir() and p.name == "agent":
            found.append(p)
        elif p.is_dir():
            found.extend(sorted(d for d in p.rglob("agent") if d.is_dir()))
    return found


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("paths", nargs="+", type=Path, help="run folders (runs/<run>) or trial agent folders")
    ap.add_argument("-o", "--out", type=Path, default=Path("."), help="where the CSV files go (default: here)")
    args = ap.parse_args(argv)

    folders = agent_folders(args.paths)
    if not folders:
        print("no agent/ folder found under " + ", ".join(map(str, args.paths)), file=sys.stderr)
        return 1
    events_rows, phase_rows, task_rows, cache = [], [], [], {}
    for agent in folders:
        source, events = trial_events(agent)
        key = where(agent)
        if not events:
            print(f"skipped {agent}: no Völund log lines", file=sys.stderr)
            continue
        events_rows.extend(event_row(key, source, i, e) for i, e in enumerate(events))
        phases, trial = summarise(events)
        base = {k: key[k] for k in ("run", "attempt", "task", "trial")}
        for name, p in phases.items():
            if p["visits"] or p["tool_calls"] or p["llm_calls"]:
                phase_rows.append({**base, "level": trial["level"], "phase": name, **p,
                                   "seconds": round(p["seconds"], 3)})
        result = {}
        if key["_run_dir"] is not None:
            rel = os.path.normpath(os.path.relpath(key["_trial_dir"], key["_run_dir"]))
            result = scores(key["_run_dir"], cache).get(rel, {})
        totals = {k: sum(p[k] for p in phases.values()) for k in ("tool_calls", "llm_calls", *TOKEN_FIELDS)}
        task_rows.append({
            **base, **trial, **totals,
            "path": ">".join(trial["path"]),
            "seconds": round(trial["seconds"], 3),
            "score": cell(result.get("score")),
            "measured": cell(result.get("measured")),
            "status": cell(result.get("status")),
            "source": source,
        })
    args.out.mkdir(parents=True, exist_ok=True)
    write(args.out / "events.csv", EVENT_COLUMNS, events_rows)
    write(args.out / "phases.csv", PHASE_COLUMNS, phase_rows)
    write(args.out / "tasks.csv", TASK_COLUMNS, task_rows)
    print(f"{len(task_rows)} trial(s), {len(events_rows)} event(s) -> {args.out}/events.csv, phases.csv, tasks.csv")
    return 0


if __name__ == "__main__":
    sys.exit(main())
