"""Drive the ocd TUI in a pseudo-terminal against the mock provider and print screens at chosen moments.

usage:
  tui_capture.py --bin target/debug/ocd --work DIR --script turns.json [--cols 100] [--rows 30]
                 [--key 2:hello] [--key 3:<CR>] [--snap 5] [--snap 9.5] [--duration 30] [--mini] [--config JSON]

Everything runs isolated: DIR/project holds `opencode.json` (provider `mock` → the mock on a free port),
DIR/state holds XDG_* / HOME / OPENCODE_TEST_HOME (the background service is disabled), and the TUI runs with
`--standalone`. `--key T:TEXT` writes TEXT at T seconds (`<CR>`, `<ESC>` are escapes; wait > 1 s first, the colour
probe swallows early input). `--snap T` prints the screen as it was T seconds after start. The raw output and a
`script`-style timing log are kept in DIR (tui.raw, tui.timing). Processes started here are killed by PID at the end.
"""
import argparse
import fcntl
import json
import re
import os
import select
import shutil
import struct
import subprocess
import sys
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from parity.harness import isolated_env  # noqa: E402
from parity.mock import Mock  # noqa: E402
import vt_render  # noqa: E402


def screen_at(raw, marks, at, cols, rows):
    end = 0
    for t, off in marks:
        if t > at:
            break
        end = off
    text = raw[:end].decode("utf-8", "replace")
    # A timing chunk can end inside an escape sequence: drop the incomplete tail.
    esc = text.rfind("\x1b")
    if esc != -1 and not re.match(r"\x1b(\[[?>=]?[0-9;:]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|[^\[\]])", text[esc:]):
        text = text[:esc]
    return vt_render.render(text, cols, rows)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--script", required=True)
    ap.add_argument("--cols", type=int, default=100)
    ap.add_argument("--rows", type=int, default=30)
    ap.add_argument("--key", action="append", default=[])
    ap.add_argument("--snap", action="append", type=float, default=[])
    ap.add_argument("--duration", type=float, default=30)
    ap.add_argument("--mini", action="store_true")
    ap.add_argument("--ready", default=None, help="key and snap times count from when the screen first shows this text")
    ap.add_argument("--config", default=None, help="cli.json (TUI config) as JSON")
    a = ap.parse_args()

    work = os.path.abspath(a.work)
    project, state = os.path.join(work, "project"), os.path.join(work, "state")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(project)
    with open(a.script) as f:
        turns = json.load(f)
    mock = Mock(0, turns)
    cfg = {"model": "mock/m1", "providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{mock.port}/v1", "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
    with open(os.path.join(project, "opencode.json"), "w") as f:
        json.dump(cfg, f, indent=2)
    with open(os.path.join(project, "hello.txt"), "w") as f:
        f.write("hi\n")
    env = isolated_env(state)
    env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor"})
    env.pop("NO_COLOR", None)
    if a.config:
        tui = os.path.join(env["XDG_CONFIG_HOME"], "opencode")
        os.makedirs(tui, exist_ok=True)
        with open(os.path.join(tui, "cli.json"), "w") as f:
            f.write(a.config)
    args = [os.path.abspath(a.bin)] + (["mini"] if a.mini else []) + ["--standalone"]
    # Our own pty (not `script`): every byte is timestamped as it is read.
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))
    proc = subprocess.Popen(args, cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave,
                            start_new_session=True, preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0))
    os.close(slave)
    start = t0 = time.time()
    chunks = []
    done = threading.Event()

    def pump():
        while not done.is_set():
            r, _, _ = select.select([master], [], [], 0.1)
            if not r:
                continue
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            chunks.append((time.time() - t0, data))

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    keys = sorted((float(k.split(":", 1)[0]), k.split(":", 1)[1]) for k in a.key)
    base = 0.0
    if a.ready:
        while time.time() - start < 60:
            time.sleep(0.2)
            screen = vt_render.render(b"".join(c for _, c in chunks).decode("utf-8", "replace"), a.cols, a.rows)
            if a.ready in screen:
                base = time.time() - start
                break
        print(f"===== ready ({a.ready!r}) after {base:.1f}s; times below count from there =====")
    start += base
    try:
        for at, text in keys:
            time.sleep(max(0.0, start + at - time.time()))
            os.write(master, text.replace("<CR>", "\r").replace("<ESC>", "\x1b").encode())
        time.sleep(max(0.0, start + a.duration - time.time()))
    finally:
        # ctrl+c twice exits the TUI; then make sure the process we started is gone.
        try:
            os.write(master, b"\x03")
            time.sleep(0.3)
            os.write(master, b"\x03")
        except OSError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        done.set()
        reader.join(timeout=2)
        os.close(master)
        mock.close()
    raw = b"".join(c for _, c in chunks)
    marks, off = [], 0
    for t, c in chunks:
        off += len(c)
        marks.append((t, off))
    with open(os.path.join(work, "tui.raw"), "wb") as f:
        f.write(raw)
    with open(os.path.join(work, "tui.timing"), "w") as f:
        prev = 0.0
        for t, c in chunks:
            f.write(f"{t - prev:.6f} {len(c)}\n")
            prev = t
    for at in a.snap:
        print(f"===== t={at:.1f}s ({a.cols}x{a.rows}) =====")
        print(screen_at(raw, marks, base + at, a.cols, a.rows))
    used = [u for u in mock.used] if hasattr(mock, "used") else []
    print(f"===== mock turns used: {len(used)} =====", file=sys.stderr)


if __name__ == "__main__":
    main()
