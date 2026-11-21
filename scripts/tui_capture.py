"""Drive the ocd TUI in a pseudo-terminal against the mock provider and print screens at chosen moments.

usage:
  tui_capture.py --bin target/debug/ocd --work DIR --script turns.json [--cols 100] [--rows 30]
                 [--key 2:hello] [--key 3:<CR>] [--snap 5] [--snap 9.5] [--duration 30] [--mini] [--config JSON]
                 [--seed-session ses_…:TITLE] [--project-config JSON]
                 [--seed-prompt ses_…:JSON] [--session ses_…] [--no-standalone] [--file NAME:BASE64]
                 [--env K=V] [--terminal kitty|sixel|plain] [--pixels WxH]

Everything runs isolated: DIR/project holds `opencode.json` (provider `mock` → the mock on a free port),
DIR/state holds XDG_* / HOME / OPENCODE_TEST_HOME (the background service is disabled), and the TUI runs with
`--standalone`. `--key T:TEXT` writes TEXT at T seconds (`<CR>`, `<ESC>` are escapes; wait > 1 s first, the colour
probe swallows early input). `--snap T` prints the screen as it was T seconds after start. The raw output and a
`script`-style timing log are kept in DIR (tui.raw, tui.timing). Processes started here are killed by PID at the end.

Terminal graphics: `--terminal` answers the startup queries like such a terminal would (`kitty`: XTVERSION
`kitty(0.40.1)` and an OK to the Kitty graphics query; `sixel`: XTVERSION `XTerm(390)` and DA1 attribute 4;
`plain`: DA1 only), `--pixels` gives the window size in pixels (the pty's pixel size and the `CSI 14 t` reply), and
`--env` sets terminal identity variables (TERM_PROGRAM, OPENTUI_IMAGE_PROTOCOL, ...). DIR/tui.graphics lists every
graphics escape written (Kitty APC `ESC _ G`, Sixel DCS `ESC P ... q`) with the cursor move before it and its payload
summarized (length and SHA-256), image ids replaced by their order of first use so two programs compare.
`--seed-prompt` sends a prompt (JSON body, `$PROJECT` replaced) to a seeded session and waits for its turn; `--file`
writes a project file (`shot.png:<base64>`); `--session` opens the TUI on a session.
"""
import argparse
import base64
import fcntl
import hashlib
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


QUERY = re.compile(rb"\x1b\[6n|\x1b\[>0q|\x1b_G[^\x1b]*\x1b\\|\x1b\[c|\x1b\[14t")


def answer(query, terminal, pixels):
    if query == b"\x1b[6n":
        return b"\x1b[1;1R"
    if terminal is None:
        return None
    if query == b"\x1b[>0q":
        return {"kitty": b"\x1bP>|kitty(0.40.1)\x1b\\", "sixel": b"\x1bP>|XTerm(390)\x1b\\"}.get(terminal)
    if query.startswith(b"\x1b_G"):
        return b"\x1b_Gi=31337;OK\x1b\\" if terminal == "kitty" and b"i=31337" in query else None
    if query == b"\x1b[14t":
        return f"\x1b[4;{pixels[1]};{pixels[0]}t".encode() if pixels[0] else None
    if query == b"\x1b[c":
        return b"\x1b[?62;4;22c" if terminal == "sixel" else b"\x1b[?62;22c"
    return None


GRAPHICS = re.compile(rb"(\x1b\[[0-9;]*[HABCDsu]|\r)*(\x1bPtmux;)?(\x1b\x1b|\x1b)(_G[^\x1b]*|P[0-9;]*q[^\x1b]*)(\x1b\x1b\\\x1b\\|\x1b\\)")


def graphics_report(raw):
    """Every Kitty and Sixel escape in `raw`, one per line: the cursor moves just before it, its control data and its
    payload as length + SHA-256 (a chunked Kitty transmission counts as one, its chunks' payloads joined). Image ids
    (`i=`) are numbered by first use."""
    ids = {}
    entries = []
    for m in GRAPHICS.finditer(raw):
        moves = re.findall(rb"\x1b\[[0-9;]*[HABCDsu]|\r", raw[m.start():m.start(4) - 1])
        body = m.group(4)
        prefix = "tmux " if m.group(2) else ""
        if body.startswith(b"_G"):
            control, _, payload = body[2:].partition(b";")
            fields = []
            for kv in control.split(b","):
                k, _, v = kv.partition(b"=")
                if k == b"i" and v != b"31337":
                    v = str(ids.setdefault(v, len(ids) + 1)).encode()
                fields.append(k + b"=" + v)
            if control.startswith(b"m=") and entries and entries[-1][3]:
                entries[-1][2].extend(payload)
                entries[-1][3] = control.startswith(b"m=1")
                continue
            more = b"m=1" in control.split(b",")
            entries.append([moves, prefix + "kitty " + b",".join(fields).decode(), bytearray(payload), more])
        else:
            head, _, payload = body.partition(b"q")
            entries.append([moves, prefix + "sixel " + head[1:].decode() + "q", bytearray(payload), False])
    out = []
    for moves, desc, payload, _ in entries:
        if payload:
            desc += f" payload={len(payload)}:{hashlib.sha256(bytes(payload)).hexdigest()[:16]}"
        out.append(" ".join(x.decode().replace("\x1b", "ESC").replace("\r", "CR") for x in moves) + (" " if moves else "") + desc)
    return "\n".join(out) + ("\n" if out else "")


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
    ap.add_argument("--project-config", default=None, help="keys merged into the project's opencode.json, as JSON")
    ap.add_argument("--seed-session", action="append", default=[], metavar="ID:TITLE",
                    help="create this session (through a `serve` of the same binary and state) before the TUI starts")
    ap.add_argument("--seed-prompt", action="append", default=[], metavar="ID:JSON",
                    help="send this prompt to a seeded session and wait for the turn to finish")
    ap.add_argument("--session", default=None, help="open the TUI on this session (`--session`)")
    ap.add_argument("--no-standalone", action="store_true",
                    help="leave out `--standalone` (the background service stays disabled, so the TUI still runs its own server)")
    ap.add_argument("--file", action="append", default=[], metavar="NAME:BASE64", help="a project file")
    ap.add_argument("--env", action="append", default=[], metavar="K=V", help="set a variable for the TUI")
    ap.add_argument("--terminal", default=None, choices=["kitty", "sixel", "plain"],
                    help="answer the graphics capability queries like this terminal")
    ap.add_argument("--pixels", default=None, metavar="WxH", help="window size in pixels")
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
    if a.project_config:
        cfg.update(json.loads(a.project_config))
    with open(os.path.join(project, "opencode.json"), "w") as f:
        json.dump(cfg, f, indent=2)
    with open(os.path.join(project, "hello.txt"), "w") as f:
        f.write("hi\n")
    for spec in a.file:
        name, data = spec.split(":", 1)
        with open(os.path.join(project, name), "wb") as f:
            f.write(base64.b64decode(data))
    env = isolated_env(state)
    env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor"})
    env.pop("NO_COLOR", None)
    if a.config:
        tui = os.path.join(env["XDG_CONFIG_HOME"], "opencode")
        os.makedirs(tui, exist_ok=True)
        with open(os.path.join(tui, "cli.json"), "w") as f:
            f.write(a.config)
    if a.seed_session:
        from parity.harness import Server  # noqa: E402
        server = Server(os.path.abspath(a.bin), project, env, "seed")
        try:
            for spec in a.seed_session:
                sid, title = spec.split(":", 1)
                status, _, body = server.request("POST", "/api/session", {"id": sid, "title": title, "location": {"directory": project}})
                if status != 200:
                    raise RuntimeError(f"seeding {sid}: {status} {body}")
            for spec in a.seed_prompt:
                sid, payload = spec.split(":", 1)
                status, _, body = server.request("POST", f"/api/session/{sid}/prompt", json.loads(payload.replace("$PROJECT", project)))
                if status != 200:
                    raise RuntimeError(f"prompting {sid}: {status} {body}")
                deadline = time.time() + 60
                while time.time() < deadline:
                    time.sleep(0.5)
                    _, _, msgs = server.request("GET", f"/api/session/{sid}/message?order=asc")
                    # The list answers `{"data": [...], "cursor": ...}`.
                    items = msgs if isinstance(msgs, list) else (msgs.get("data") or msgs.get("items") or []) if isinstance(msgs, dict) else []
                    last = items[-1] if isinstance(items, list) and items else {}
                    # A finished turn ends with an `idle` message.
                    if last.get("type") == "idle" or (last.get("type") == "assistant" and (last.get("time") or {}).get("completed") and last.get("finish") != "tool-calls"):
                        break
        finally:
            server.close()
    env.update(dict(kv.split("=", 1) for kv in a.env))
    pixels = tuple(int(v) for v in a.pixels.split("x")) if a.pixels else (0, 0)
    args = [os.path.abspath(a.bin)] + (["mini"] if a.mini else []) + ([] if a.no_standalone else ["--standalone"]) + (["--session", a.session] if a.session else [])
    # Our own pty (not `script`): every byte is timestamped as it is read.
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, pixels[0], pixels[1]))
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
            # Answer cursor position reports (DSR) like a terminal would, and with --terminal the capability
            # queries, in the order they were asked.
            for m in QUERY.finditer(data):
                reply = answer(m.group(0), a.terminal, pixels)
                if reply:
                    os.write(master, reply)

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
    with open(os.path.join(work, "tui.graphics"), "w") as f:
        f.write(graphics_report(raw))
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
