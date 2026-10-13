"""Measure how responsive the ocd TUI stays late in a long session.

usage:
  tui_latency.py --bin target/release/ocd --work DIR [--turns 4] [--lines 500] [--pace 0.005]
                 [--cols 120] [--rows 40] [--timeout 120]

Runs the TUI isolated like `tui_capture.py` (DIR/project, DIR/state, `--standalone`, the background service
disabled) against the mock provider, which streams `--turns` long markdown answers (`--lines` lines each, with code
blocks, one SSE chunk per line every `--pace` seconds), then asks a multiple-choice question (the `question` tool).
It measures, from the moment input is written to the pty until the screen shows its effect:

- each answer's end becoming visible (turn time; the stream itself takes about lines × pace),
- typing into the prompt while an answer streams (echo latency),
- the question dialog appearing after the last prompt,
- ↓ + space in the question dialog (the checkbox of the next option turning on), several times.

The screen is reconstructed incrementally from the pty output (`vt_render.Screen`). Processes started here are
killed by PID at the end.
"""
import argparse
import codecs
import fcntl
import json
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

OPTIONS = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"]


def answer(turn, lines):
    """A long markdown answer, one chunk per line, with a code block every 40 lines."""
    out = [f"## Answer {turn}\n\n"]
    k = 0
    while k < lines:
        if k % 40 == 20:
            out.append("```rust\n")
            for j in range(8):
                out.append(f"fn step_{turn}_{k}_{j}(x: u32) -> u32 {{ let y = x * {j} + {k}; y.wrapping_add({turn}) }}\n")
            out.append("```\n\n")
            k += 10
            continue
        end = "\n\n" if k % 5 == 4 else "\n"
        out.append(f"Line {k} of answer {turn}: the parser keeps **state** across `chunks`, so we check every word.{end}")
        k += 1
    out.append(f"\nEND-OF-ANSWER-{turn}\n")
    return out


class Tui:
    def __init__(self, args, cwd, env, cols, rows):
        master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen(args, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                                     start_new_session=True, preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0))
        os.close(slave)
        self.master = master
        self.screen = vt_render.Screen(cols, rows)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.lock = threading.Lock()
        self.bytes = 0
        self.done = threading.Event()
        self.reader = threading.Thread(target=self._pump, daemon=True)
        self.reader.start()

    def _pump(self):
        while not self.done.is_set():
            r, _, _ = select.select([self.master], [], [], 0.05)
            if not r:
                continue
            try:
                data = os.read(self.master, 1 << 16)
            except OSError:
                break
            if not data:
                break
            with self.lock:
                self.bytes += len(data)
                self.screen.feed(self.decoder.decode(data))

    def text(self):
        with self.lock:
            return self.screen.text()

    def send(self, s):
        os.write(self.master, s.encode())
        return time.time()

    def wait_for(self, needle, timeout, gone=False):
        """Seconds until `needle` is on screen (or off it with `gone`); None on timeout."""
        start = time.time()
        while time.time() - start < timeout:
            if (needle in self.text()) != gone:
                return time.time() - start
            time.sleep(0.005)
        return None

    def close(self):
        try:
            os.write(self.master, b"\x03")
            time.sleep(0.3)
            os.write(self.master, b"\x03")
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.done.set()
        self.reader.join(timeout=2)
        os.close(self.master)


def fmt(v):
    return "timeout" if v is None else f"{v * 1000:.0f} ms"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--turns", type=int, default=4)
    ap.add_argument("--lines", type=int, default=500)
    ap.add_argument("--pace", type=float, default=0.005)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--timeout", type=float, default=120)
    ap.add_argument("--config", default='{"animations": true}', help="cli.json (TUI config) as JSON")
    a = ap.parse_args()

    work = os.path.abspath(a.work)
    project, state = os.path.join(work, "project"), os.path.join(work, "state")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(project)
    question = {"questions": [{"question": "Which ones should I keep?", "header": "Keep", "multiple": True,
                               "options": [{"label": o, "description": f"keep {o.lower()}"} for o in OPTIONS]}]}
    turns = [{"text": answer(t + 1, a.lines), "pace": a.pace} for t in range(a.turns)]
    turns.append({"tools": [{"name": "question", "args": question}]})
    turns.append({"text": "Noted."})
    mock = Mock(0, turns)
    cfg = {"model": "mock/m1", "providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{mock.port}/v1", "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 1000000, "output": 4096}}}}}}
    with open(os.path.join(project, "opencode.json"), "w") as f:
        json.dump(cfg, f, indent=2)
    env = isolated_env(state)
    env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor"})
    env.pop("NO_COLOR", None)
    tui_cfg = os.path.join(env["XDG_CONFIG_HOME"], "opencode")
    os.makedirs(tui_cfg, exist_ok=True)
    with open(os.path.join(tui_cfg, "cli.json"), "w") as f:
        f.write(a.config)
    tui = Tui([os.path.abspath(a.bin), "--standalone"], project, env, a.cols, a.rows)
    results = {}
    try:
        ready = tui.wait_for("Mock One", 60)
        print(f"ready after {fmt(ready)}")
        time.sleep(1.5)  # the colour probe swallows early input
        for t in range(1, a.turns + 1):
            sent = tui.send(f"turn {t}\r")
            # Type while the answer streams; the prompt must echo it promptly.
            time.sleep(min(1.0, a.lines * a.pace / 2))
            typed = tui.send(f"zq{t}x")
            echo = tui.wait_for(f"zq{t}x", a.timeout)
            tui.send("\x7f" * 4)
            end = tui.wait_for(f"END-OF-ANSWER-{t}", a.timeout)
            total = None if end is None else time.time() - sent
            print(f"turn {t}: answer end visible after {fmt(total)} (stream ≈ {a.lines * a.pace:.1f} s); "
                  f"typing echo mid-stream {fmt(echo)} (typed {typed - sent:.1f} s after submit)")
            results[f"turn{t}"] = {"end": total, "echo": echo}
            if end is None:
                break
            time.sleep(1.0)
        sent = tui.send("ask me\r")
        shown = tui.wait_for("Alpha", a.timeout)
        print(f"question dialog shown after {fmt(shown)}")
        results["question"] = shown
        lat = []
        if shown is not None:
            time.sleep(0.5)
            for label in OPTIONS[1:]:
                tui.send("\x1b[B ")
                d = tui.wait_for(f"[✓] {label}", 30)
                lat.append(d)
                print(f"↓ + space → [✓] {label}: {fmt(d)}")
                time.sleep(0.4)
        results["keys"] = lat
        print(f"pty output: {tui.bytes / 1e6:.1f} MB")
    finally:
        tui.close()
        mock.close()
    with open(os.path.join(work, "latency.json"), "w") as f:
        json.dump(results, f, indent=2)
    with open(os.path.join(work, "screen.txt"), "w") as f:
        f.write(tui.text())


if __name__ == "__main__":
    main()
