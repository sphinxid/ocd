#!/usr/bin/env python3
"""Differential parity suite: run named scenarios against upstream opencode and the Rust port, diff the dumps.

usage:
  parity.py [--upstream BIN] [--rust BIN] [--out DIR] [--list] [--group G] [scenario ...]

Each scenario runs once per binary with identical project/state paths (recreated before each run) and a fresh
scriptable mock provider (scripts/parity/mock.py) on one port shared by both runs. Outputs land in
<out>/<scenario>/{upstream,rust}.txt and diff.txt; the summary lists IDENTICAL / DIFF per scenario.
Scenario names may be glob patterns (e.g. 'session_*'). See docs/PARITY.md.
"""
import argparse
import difflib
import fnmatch
import os
import socket
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from parity import harness  # noqa: E402
from parity.mock import Mock  # noqa: E402
from parity.registry import SCENARIOS  # noqa: E402
from parity import scenarios  # noqa: E402,F401  (registers scenarios)

SCRATCH = os.environ.get("OCD_SCRATCH", "/tmp/ocd-scratch")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def default_rust():
    for name in ("ocd", "opencode"):
        p = os.path.join(ROOT, "target", "debug", name)
        if os.path.exists(p):
            return p
    return os.path.join(ROOT, "target", "debug", "ocd")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def run_one(sc, binaries, out_root, port):
    work = os.path.join(out_root, "work", sc.name)
    dumps = {}
    for side, binary in binaries:
        mock = Mock(port, hold_for_title=sc.hold_for_title)
        ctx = harness.Ctx(side, binary, work, mock, {})
        started = time.time()
        try:
            sc.fn(ctx)
        except Exception:
            ctx.rec("EXCEPTION", traceback.format_exc().strip().splitlines()[-1])
            if os.environ.get("PARITY_TRACE"):
                traceback.print_exc()
        finally:
            ctx.stop()
            mock.close()
        prog = os.path.basename(binary) if side == "rust" else None
        dumps[side] = harness.dump(ctx, prog_name=prog if prog != "opencode" else None)
        d = os.path.join(out_root, sc.name)
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"{side}.txt"), "w") as f:
            f.write("\n".join(dumps[side]) + "\n")
        dumps[side + "_time"] = time.time() - started
    diff = list(difflib.unified_diff(dumps["upstream"], dumps["rust"], "upstream", "rust", lineterm="", n=2))
    with open(os.path.join(out_root, sc.name, "diff.txt"), "w") as f:
        f.write("\n".join(diff) + "\n" if diff else "IDENTICAL\n")
    return diff, dumps


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--upstream", default=os.environ.get(
        "PARITY_UPSTREAM", os.path.join(SCRATCH, "up-dist/cli-linux-x64/bin/opencode")))
    ap.add_argument("--rust", default=os.environ.get("PARITY_RUST") or default_rust())
    ap.add_argument("--out", default=os.environ.get("PARITY_OUT", os.path.join(SCRATCH, "parity-out")))
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--group", action="append", default=[])
    ap.add_argument("--quiet", "-q", action="store_true", help="only print the summary, not the diffs")
    ap.add_argument("names", nargs="*")
    args = ap.parse_args()

    if args.list:
        for sc in SCENARIOS.values():
            print(f"{sc.name:32} {sc.group:10} {sc.doc}")
        return 0
    selected = []
    for sc in SCENARIOS.values():
        if args.group and sc.group not in args.group:
            continue
        if args.names and not any(fnmatch.fnmatch(sc.name, n) for n in args.names):
            continue
        selected.append(sc)
    if not selected:
        print("no scenarios selected", file=sys.stderr)
        return 2
    os.makedirs(args.out, exist_ok=True)
    port = free_port()
    summary = []
    for sc in selected:
        diff, dumps = run_one(sc, [("upstream", args.upstream), ("rust", args.rust)], args.out, port)
        status = "IDENTICAL" if not diff else f"DIFF ({sum(1 for l in diff if l[:1] in '+-') - 2} lines)"
        summary.append((sc.name, status))
        print(f"### {sc.name}: {status}  [upstream {dumps['upstream_time']:.1f}s, rust {dumps['rust_time']:.1f}s]",
              flush=True)
        if diff and not args.quiet:
            print("\n".join(diff), flush=True)
    print("\nSUMMARY")
    for name, status in summary:
        print(f"  {name:32} {status}")
    return 0 if all(s == "IDENTICAL" for _, s in summary) else 1


if __name__ == "__main__":
    sys.exit(main())
