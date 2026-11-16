"""Tests for logs_to_csv.py: python3 -m unittest discover -s tools"""

import csv
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import logs_to_csv  # noqa: E402


def line(prefix, **event):
    return prefix + json.dumps(event) + "\n"


# A phase-driver trial as serve.log holds it: Völund's lines among other stderr output.
DRIVER = "".join([
    "volund 0.1.0 (protocol 1) listening on 0.0.0.0:8000, model m, ablation A6\n",
    line("volund: ", t=0.0, phase="PROBE", kind="start", level="A6", model="claude-opus-5-5"),
    line("volund: ", t=2.0, phase="PROBE", kind="transition", event="phase_ended", to="INTAKE",
         reason="probe complete", effects=[]),
    line("volund: ", t=3.0, phase="INTAKE", kind="llm", input_tokens=100, output_tokens=10, cache_read_tokens=0),
    line("volund: ", t=4.0, phase="INTAKE", kind="tool", name="world_deliverable", args='{"path":"a.md"}'),
    line("volund: ", t=5.0, phase="INTAKE", kind="transition", event="phase_ended", to="WRITE",
         reason="no constraints: nothing to model", effects=[]),
    line("volund: ", t=6.0, phase="WRITE", kind="llm", input_tokens=200, output_tokens=20, cache_read_tokens=150),
    "volund: mcp apps: 1 tools\n",
    line("volund: ", t=9.0, phase="WRITE", kind="transition", event="phase_ended", to="CHECK",
         reason="deliverables written", effects=[]),
    line("volund: ", t=9.5, phase="CHECK", kind="check", checks=[
        {"name": "a.md", "passed": True, "detail": "9 bytes"},
        {"name": logs_to_csv.STRAY_CHECK, "passed": False, "detail": "1 file(s) added"},
    ]),
    line("volund: ", t=9.5, phase="CHECK", kind="transition", event="phase_ended", to="FIX",
         reason="checks failing", effects=["snapshot"]),
    line("volund: ", t=12.0, phase="FIX", kind="transition", event="phase_ended", to="CHECK",
         reason="fix applied", effects=[]),
    line("volund: ", t=12.5, phase="CHECK", kind="transition", event="phase_ended", to="FINALIZE",
         reason="fix lowered quality: rolled back", effects=["rollback"]),
    line("volund: ", t=14.0, phase="FINALIZE", kind="llm", input_tokens=50, output_tokens=5, cache_read_tokens=0),
])

# A single-loop trial (A1) as the kit's <harness>.txt holds it.
LOOP = "".join([
    line("harness: ", t=0.0, phase="LOOP", kind="start", level="A1", model="claude-opus-5-5"),
    "harness: cancel received (timeout)\n",
    line("harness: ", t=1.0, phase="LOOP", kind="llm", input_tokens=10, output_tokens=1, cache_read_tokens=0),
    line("harness: ", t=4.0, phase="LOOP", kind="tool", name="bash", args='{"command":"ls"}'),
    "serve status=done\n",
])


def read(path):
    with open(path, newline="", encoding="utf-8") as f:
        return list(csv.DictReader(f))


class LogsToCsv(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.run_dir = root / "runs" / "r1"
        driver = self.run_dir / "attempt1" / "jobs" / "dev" / "beam-sizing__ab12" / "agent"
        loop = self.run_dir / "attempt1" / "jobs" / "dev" / "memo__cd34" / "agent"
        empty = self.run_dir / "attempt1" / "jobs" / "dev" / "other__ef56" / "agent"
        for d in (driver, loop, empty):
            d.mkdir(parents=True)
        (driver / "serve.log").write_text(DRIVER, encoding="utf-8")
        (driver / "volund.txt").write_text("harness: not read when serve.log has events\n", encoding="utf-8")
        (loop / "serve.log").write_text("volund 0.1.0 listening\n", encoding="utf-8")
        (loop / "volund.txt").write_text(LOOP, encoding="utf-8")
        (empty / "serve.log").write_text("nothing here\n", encoding="utf-8")
        results = {"tasks": [
            {"id": "beam-sizing", "trial": "attempt1/jobs/dev/beam-sizing__ab12", "score": 0.42,
             "measured": True, "status": "ok"},
        ]}
        (self.run_dir / "results.json").write_text(json.dumps(results), encoding="utf-8")
        self.out = root / "out"
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()) as err:
            self.code = logs_to_csv.main([str(self.run_dir), "-o", str(self.out)])
        self.err = err.getvalue()

    def tearDown(self):
        self.tmp.cleanup()

    def test_every_event_becomes_a_row_from_the_right_file(self):
        self.assertEqual(self.code, 0)
        events = read(self.out / "events.csv")
        self.assertEqual(len(events), 12 + 3, "12 driver events, 3 loop events; other lines skipped")
        first = events[0]
        self.assertEqual(
            (first["run"], first["attempt"], first["task"], first["trial"], first["source"], first["kind"]),
            ("r1", "attempt1", "beam-sizing", "beam-sizing__ab12", "serve.log", "start"),
        )
        self.assertEqual(json.loads(first["data"]), {"level": "A6", "model": "claude-opus-5-5"})
        check = next(e for e in events if e["kind"] == "check")
        self.assertEqual(check["failed_checks"], logs_to_csv.STRAY_CHECK)
        loop = [e for e in events if e["task"] == "memo"]
        self.assertEqual({e["source"] for e in loop}, {"volund.txt"})
        self.assertIn("skipped", self.err, "the trial without Völund lines is reported")

    def test_phases_add_up_time_calls_and_tokens(self):
        phases = {(p["task"], p["phase"]): p for p in read(self.out / "phases.csv")}
        write = phases[("beam-sizing", "WRITE")]
        self.assertEqual((write["visits"], write["seconds"], write["llm_calls"]), ("1", "4.0", "1"))
        self.assertEqual((write["input_tokens"], write["cache_read_tokens"]), ("200", "150"))
        check = phases[("beam-sizing", "CHECK")]
        self.assertEqual((check["visits"], check["seconds"]), ("2", "1.0"))
        final = phases[("beam-sizing", "FINALIZE")]
        self.assertEqual((final["visits"], final["seconds"], final["output_tokens"]), ("1", "1.5", "5"))
        loop = phases[("memo", "LOOP")]
        self.assertEqual((loop["visits"], loop["seconds"], loop["tool_calls"], loop["level"]), ("1", "4.0", "1", "A1"))

    def test_tasks_carry_the_path_the_reason_and_the_score(self):
        tasks = {t["task"]: t for t in read(self.out / "tasks.csv")}
        self.assertEqual(set(tasks), {"beam-sizing", "memo"})
        t = tasks["beam-sizing"]
        self.assertEqual(t["path"], "PROBE>INTAKE>WRITE>CHECK>FIX>CHECK>FINALIZE")
        self.assertEqual(t["last_reason"], "fix lowered quality: rolled back")
        self.assertEqual((t["fix_rounds"], t["rollbacks"], t["stray_checks_failed"]), ("1", "1", "1"))
        self.assertEqual((t["score"], t["measured"], t["status"]), ("0.42", "True", "ok"))
        self.assertEqual((t["llm_calls"], t["input_tokens"], t["seconds"]), ("3", "350", "14.0"))
        self.assertEqual((t["level"], t["model"]), ("A6", "claude-opus-5-5"))
        m = tasks["memo"]
        self.assertEqual((m["path"], m["score"], m["level"]), ("LOOP", "", "A1"))

    def test_no_agent_folder_is_an_error(self):
        with redirect_stderr(io.StringIO()):
            self.assertEqual(logs_to_csv.main([self.tmp.name + "/nowhere", "-o", str(self.out)]), 1)


if __name__ == "__main__":
    unittest.main()
