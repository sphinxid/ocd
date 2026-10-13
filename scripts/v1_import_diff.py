#!/usr/bin/env python3
"""Differential test of the v1 -> v2 history import: upstream opencode v2 vs ocd on copies of the same v1 DB.

usage: v1_import_diff.py V1_DB [V1_DB ...] [--upstream BIN] [--rust BIN] [--out DIR] [--next NEXT_DB]

For every binary the DB is copied into a fresh isolated state root (XDG_* / HOME under --out), `serve --port 0` is
started, the experimental v1-migration status endpoint is polled until it reports `completed` (or `error`), the
server is stopped, and session_v2 / session_message / event_sequence / project / kv / event are dumped. Only
`time_*` values that are set to "now" by the import (kv timestamps, a freshly inserted global project row) are
normalized. Prints IDENTICAL or the unified diff per input DB.
"""
import argparse
import base64
import difflib
import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.request

SCRATCH = os.environ.get("OCD_SCRATCH", "/tmp/ocd-scratch")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PASSWORD = "v1-import-diff"


def isolated_env(root, db):
    env = {k: v for k, v in os.environ.items() if not k.startswith("OPENCODE")}
    for key, sub in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                     ("XDG_CACHE_HOME", "cache")]:
        env[key] = os.path.join(root, sub)
        os.makedirs(env[key], exist_ok=True)
    home = os.path.join(root, "home")
    os.makedirs(home, exist_ok=True)
    env.update({
        "HOME": home,
        "OPENCODE_TEST_HOME": home,
        "OPENCODE_DB": db,
        "OPENCODE_PASSWORD": PASSWORD,
        "OPENCODE_DISABLE_MODELS_FETCH": "1",
        "OPENCODE_DISABLE_FILEWATCHER": "1",
        "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "GIT_CONFIG_NOSYSTEM": "1",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "TZ": "UTC",
    })
    return env


def run_import(binary, v1db, root, next_db=None):
    if os.path.exists(root):
        shutil.rmtree(root)
    data = os.path.join(root, "data", "opencode")
    os.makedirs(data)
    db = os.path.join(data, "opencode.db")
    shutil.copyfile(v1db, db)
    if next_db:
        shutil.copyfile(next_db, os.path.join(data, "opencode-next.db"))
    cwd = os.path.join(root, "cwd")
    os.makedirs(cwd)
    env = isolated_env(root, db)
    proc = subprocess.Popen([binary, "serve", "--port", "0"], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    url = None
    deadline = time.time() + 60
    out = []
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            break
        out.append(line)
        m = re.search(r"https?://[\w.:\[\]]+", line)
        if m and "listening" in line:
            url = m.group(0)
            break
    statuses = []
    try:
        if not url:
            raise RuntimeError("server did not report a URL:\n" + "".join(out))
        auth = "Basic " + base64.b64encode(f"opencode:{PASSWORD}".encode()).decode()
        deadline = time.time() + 300
        while time.time() < deadline:
            req = urllib.request.Request(url + "/api/experimental/migration/v1", headers={"authorization": auth})
            with urllib.request.urlopen(req, timeout=30) as r:
                status = json.loads(r.read())
            if not statuses or statuses[-1] != status:
                statuses.append(status)
            if status.get("status") in ("completed", "error"):
                break
            time.sleep(0.05)
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    return db, statuses


def dump(db):
    c = sqlite3.connect(db)
    c.row_factory = sqlite3.Row
    lines = []

    def table(name, order, normalize=()):
        cols = [r[1] for r in c.execute(f"PRAGMA table_info({name})")]
        lines.append(f"## {name} ({', '.join(cols)})")
        for row in c.execute(f"SELECT * FROM {name} ORDER BY {order}"):
            vals = []
            for col in cols:
                v = row[col]
                if col in normalize and v is not None:
                    v = "<now>"
                vals.append(f"{col}={v!r}")
            lines.append("  " + " | ".join(vals))

    table("session_v2", "id")
    table("session_message", "session_id, seq")
    table("event_sequence", "aggregate_id")
    # The global project row is inserted with Date.now() when it does not exist yet.
    cols = [r[1] for r in c.execute("PRAGMA table_info(project)")]
    lines.append(f"## project ({', '.join(cols)})")
    v1_global = None
    for row in c.execute("SELECT * FROM project ORDER BY id"):
        vals = []
        for col in cols:
            v = row[col]
            if row["id"] == "global" and col.startswith("time_") and v is not None:
                v = "<time>"
            vals.append(f"{col}={v!r}")
        lines.append("  " + " | ".join(vals))
    table("kv", "key", normalize=("time_created", "time_updated"))
    lines.append("## event count: %d" % c.execute("SELECT COUNT(*) FROM event").fetchone()[0])
    c.close()
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dbs", nargs="+")
    ap.add_argument("--upstream", default=os.path.join(SCRATCH, "up-dist/cli-linux-x64/bin/opencode"))
    ap.add_argument("--rust", default=os.path.join(ROOT, "target/debug/ocd"))
    ap.add_argument("--out", default=os.path.join(SCRATCH, "v1gen/diff"))
    ap.add_argument("--next", default=None, help="optional opencode-next.db placed next to the DB")
    a = ap.parse_args()
    failed = 0
    for v1db in a.dbs:
        name = os.path.splitext(os.path.basename(v1db))[0]
        dumps = {}
        for label, binary in (("upstream", a.upstream), ("rust", a.rust)):
            root = os.path.join(a.out, name, label)
            db, statuses = run_import(binary, v1db, root, a.next)
            text = dump(db)
            dumps[label] = text
            with open(os.path.join(a.out, name, label + ".txt"), "w") as f:
                f.write(text)
            final = statuses[-1] if statuses else None
            print(f"{name}: {label} final status {json.dumps(final)} (saw {len(statuses)} distinct statuses)")
        diff = list(difflib.unified_diff(dumps["upstream"].splitlines(), dumps["rust"].splitlines(), "upstream",
                                         "rust", lineterm="", n=1))
        with open(os.path.join(a.out, name, "diff.txt"), "w") as f:
            f.write("\n".join(diff) + "\n")
        if diff:
            failed += 1
            print(f"{name}: DIFF ({len(diff)} lines) -> {os.path.join(a.out, name, 'diff.txt')}")
            print("\n".join(diff[:80]))
        else:
            print(f"{name}: IDENTICAL")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
