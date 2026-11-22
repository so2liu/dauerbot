"""Exercise real HERDR panes with a deterministic, interactive custom CLI."""

import argparse
import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import sys


spec = importlib.util.spec_from_file_location(
    "manager_smoke", Path(__file__).with_name("manager-smoke.py")
)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

WORKER = r"""
import json
import os
from pathlib import Path
import re
import sys
import time

for line in sys.stdin:
    match = re.search(r'Read ("[^"]+") .*? as ("[^"]+")', line)
    if not match:
        continue
    task, result = map(lambda value: Path(json.loads(value)), match.groups())
    tag = re.search(r"parallel-task-[ab]", task.read_text()).group()
    receipt = Path.cwd() / (tag + ".start")
    fd = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(fd)
    temporary_receipt = receipt.with_suffix(".pending")
    with temporary_receipt.open("w") as handle:
        json.dump({"start_ns": time.monotonic_ns(), "pid": os.getpid(),
                   "generation": result.parent.parent.name, "run": result.parent.name}, handle)
    temporary_receipt.replace(receipt)
    release = Path.cwd() / (tag + ".release")
    deadline = time.monotonic() + 40
    while not release.exists():
        if time.monotonic() > deadline:
            raise TimeoutError(tag)
        time.sleep(0.02)
    (Path.cwd() / (tag + ".end")).write_text(str(time.monotonic_ns()))
    temporary = result.with_suffix(".new")
    temporary.write_text(f"Status: success\n\n## Summary\n{tag}\n\n## Changes\nNone\n\n## Evidence\n{tag}.start\n")
    temporary.replace(result)
"""


def run(binary, quota):
    h = smoke.Harness(binary)
    try:
        if quota is not None:
            subprocess.run([str(h.binary), "--data-dir", str(h.run), "init"],
                           env=h.env, check=True, capture_output=True, timeout=20)
            config_file = h.run / "config.json"
            config = json.loads(config_file.read_text())
            config["manager_workers_per_cli"] = quota
            config_file.write_text(json.dumps(config))
        program = h.workspace / "worker.py"
        program.write_text(WORKER)
        h.boot()
        initial = h.api("/manager/provider")
        h.api("/manager/provider", {
            "base_url": f"http://127.0.0.1:{h.model.server_port}/v1",
            "model": "fixture-manager", "api_key": "manager-fixture-secret",
            "expected_revision": initial["revision"],
        }, "PUT")
        interpreter = Path(sys.executable).resolve()
        # Framework builds may exec an inner Mach-O; name the actual executable
        # so the foreground process check sees the same binary that we launch.
        framework_app = interpreter.parent.parent / "Resources/Python.app/Contents/MacOS/Python"
        if framework_app.is_file():
            interpreter = framework_app
        project = h.api("/projects", {
            "name": "Parallel worker fixture", "path": str(h.workspace),
            "agent": "custom", "extra_args": [str(interpreter), "-u", str(program)],
        })
        route = f"/projects/{project['id']}"
        h.api(route + "/bot", {"app_id": "cli_fixture", "app_secret": "fixture-secret"}, "PUT")
        smoke.fixture.wait_for(lambda: smoke.fixture.FIXTURE.connections, "no fixture SDK connection")
        tags = ["parallel-task-a", "parallel-task-b"]
        smoke.MODEL.script(*[
            {"tool": "delegate", "id": tag, "arguments": {"target_cli": "custom", "instructions": tag}}
            for tag in tags
        ])
        smoke.fixture.FIXTURE.emit(smoke.fixture.message("om_parallel_fixture", "Start both fixture tasks."))

        def entries():
            with sqlite3.connect(h.run / "bridge.sqlite3") as db:
                return db.execute(
                    """SELECT json_extract(e.payload,'$.instructions'),e.status,
                              json_extract(e.payload,'$.target_generation'),r.id,r.status,
                              json_extract(e.payload,'$.worker_result.summary')
                       FROM work_events e LEFT JOIN work_runs r ON r.event_id=e.id
                       WHERE e.source='manager_worker' ORDER BY e.rowid"""
                ).fetchall()

        def receipt(tag):
            path = h.workspace / (tag + ".start")
            if path.exists() and path.stat().st_size:
                return json.loads(path.read_text())

        first = smoke.fixture.wait_for(lambda: receipt(tags[0]), "first real worker did not start", timeout=35)
        if quota == 1:
            def waiting():
                rows = entries()
                return len(rows) == 2 and rows[1][0:4] == (tags[1], "worker_waiting", None, None)
            smoke.fixture.wait_for(waiting, "second task was not durably queued without a seat")
            assert receipt(tags[1]) is None
            assert entries()[0][2:4] == (first["generation"], first["run"])
            with sqlite3.connect(h.run / "bridge.sqlite3") as db:
                accepted = db.execute(
                    """SELECT json_extract(result,'$.status'),json_extract(result,'$.work_queued'),
                              json_extract(result,'$.assignment_pending')
                       FROM manager_effects WHERE name='delegate' ORDER BY rowid"""
                ).fetchall()
            assert accepted == [("queued", 1, 1), ("queued", 1, 1)]
            print(json.dumps({"quota": quota, "durable_waiting": entries()[1], "delegate_receipts": accepted}),
                  flush=True)
            (h.workspace / (tags[0] + ".release")).touch()
        second = smoke.fixture.wait_for(lambda: receipt(tags[1]), "second real worker did not start", timeout=35)
        if quota is None:
            assert first["generation"] != second["generation"] and first["pid"] != second["pid"]
            assert all(not (h.workspace / (tag + ".end")).exists() for tag in tags)
            active = entries()
            assert len(active) == 2 and all(row[4] in ("submitted", "running") for row in active)
            (h.workspace / (tags[0] + ".release")).touch()
        else:
            assert first["generation"] == second["generation"]
        (h.workspace / (tags[1] + ".release")).touch()

        def settled():
            rows = entries()
            return rows if len(rows) == 2 and all(row[4] == "settled" and row[0] == row[5] for row in rows) else None

        rows = smoke.fixture.wait_for(settled, "generation-scoped results did not settle", timeout=35)
        windows = []
        for row, started in zip(rows, (first, second)):
            assert row[2:4] == (started["generation"], started["run"])
            ended = int((h.workspace / (row[0] + ".end")).read_text())
            windows.append({**started, "end_ns": ended})
        overlap = min(w["end_ns"] for w in windows) - max(w["start_ns"] for w in windows)
        assert (overlap > 0) if quota is None else (overlap <= 0)
        print(json.dumps({"quota": quota, "overlap_ns": overlap, "workers": windows,
                          "results": rows}, indent=2), flush=True)
    except Exception:
        print(f"Fixture retained at {h.run}", file=sys.stderr)
        raise
    finally:
        h.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    run(args.binary, None)
    run(args.binary, 1)
