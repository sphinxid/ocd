"""Scriptable OpenAI-compatible chat-completions mock (SSE).

A script is a list of *turns*. Every non-title request consumes the first unconsumed turn whose optional `match`
substring occurs in the request body (turns without `match` match anything). Title-generation requests (system prompt
contains "title generator") are answered with `title` and never consume a turn. When the script is exhausted the mock
answers "(script exhausted)" so over-calls show up in the dumps.

Turn fields (all optional):
  text: str | [str]          assistant text, one SSE chunk per list item
  reasoning: str | [str]     `reasoning_content` deltas sent before the text
  tools: [{name, args, id}]  tool calls (args: dict or raw string); several entries = parallel calls
  finish: str                finish_reason (default "tool_calls" when tools are present, else "stop")
  usage: {prompt_tokens, completion_tokens, ...}   final usage chunk (default 10/5)
  status: int                answer with this HTTP status instead of a stream; `body` (JSON) and `headers` apply
  cut: true                  send the content chunks, then drop the connection (no finish_reason, no [DONE])
  delay: float               seconds to wait before answering
  hang: true                 never answer (until the client disconnects or the mock stops)
  match: str                 only consume this turn for requests whose JSON body contains the substring
"""
import json
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def _chunk(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode()


def classify(body):
    text = json.dumps(body)
    if "title generator" in text:
        return "title"
    return "main"


class Mock:
    def __init__(self, port=0, script=None, title="Smoke test", hold_for_title=0.0):
        self.lock = threading.Lock()
        self.script = []
        self.used = []
        self.requests = []
        self.title = title
        # When > 0, the first main request waits (up to this many seconds) until a title request has been answered,
        # plus a short settle gap. This makes the title/step ordering deterministic for both binaries.
        self.hold_for_title = hold_for_title
        self.title_done = threading.Event()
        self.title_done_at = 0.0
        self.stopping = threading.Event()
        self.set_script(script or [])
        mock = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                mock._record("GET", self.path, None, dict(self.headers))
                self.send_response(404)
                self.end_headers()

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("content-length") or 0))
                try:
                    body = json.loads(raw)
                except Exception:
                    body = raw.decode("utf-8", "replace")
                kind = classify(body)
                turn = mock._take(kind, body)
                mock._record(kind, self.path, body, dict(self.headers))
                try:
                    mock._answer(self, kind, turn)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def set_script(self, script):
        with self.lock:
            self.script = list(script)
            self.used = [False] * len(self.script)
            self.requests = []
            self.title_done.clear()

    def _record(self, kind, path, body, headers):
        with self.lock:
            self.requests.append({"kind": kind, "path": path, "body": body, "headers": headers, "at": time.time()})

    def _take(self, kind, body):
        if kind == "title":
            return {"text": self.title}
        text = json.dumps(body)
        with self.lock:
            for i, turn in enumerate(self.script):
                if self.used[i]:
                    continue
                if turn.get("match") and turn["match"] not in text:
                    continue
                self.used[i] = True
                return turn
        return {"text": "(script exhausted)"}

    def main_requests(self):
        return [r for r in self.requests if r["kind"] == "main"]

    def _answer(self, h, kind, turn):
        if kind == "main" and self.hold_for_title > 0 and self.title_done.wait(self.hold_for_title):
            # Give the binary time to commit session.renamed before the step produces events.
            time.sleep(max(0.0, self.title_done_at + 0.5 - time.time()))
        if turn.get("delay"):
            time.sleep(turn["delay"])
        if turn.get("hang"):
            while not self.stopping.is_set():
                time.sleep(0.05)
            return
        if turn.get("status"):
            payload = json.dumps(turn.get("body", {"error": {"message": "mock error", "type": "server_error"}})).encode()
            h.send_response(turn["status"])
            h.send_header("content-type", "application/json")
            for k, v in (turn.get("headers") or {}).items():
                h.send_header(k, str(v))
            h.send_header("content-length", str(len(payload)))
            h.end_headers()
            h.wfile.write(payload)
            return
        h.send_response(200)
        h.send_header("content-type", "text/event-stream")
        h.end_headers()
        cid = "chatcmpl-mock"
        out = []
        reasoning = turn.get("reasoning")
        for piece in ([reasoning] if isinstance(reasoning, str) else (reasoning or [])):
            out.append({"id": cid, "choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": piece}}]})
        text = turn.get("text")
        for piece in ([text] if isinstance(text, str) else (text or [])):
            out.append({"id": cid, "choices": [{"index": 0, "delta": {"role": "assistant", "content": piece}}]})
        for i, tool in enumerate(turn.get("tools") or []):
            args = tool.get("args", {})
            args = args if isinstance(args, str) else json.dumps(args)
            call = {"index": i, "id": tool.get("id", f"call_{i + 1}"), "type": "function",
                    "function": {"name": tool["name"], "arguments": args}}
            out.append({"id": cid, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [call]}}]})
        for o in out:
            h.wfile.write(_chunk(o))
            h.wfile.flush()
        if turn.get("cut"):
            h.wfile.flush()
            try:
                h.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        finish = turn.get("finish") or ("tool_calls" if turn.get("tools") else "stop")
        usage = turn.get("usage") or {"prompt_tokens": 10, "completion_tokens": 5}
        usage = dict(usage)
        usage.setdefault("total_tokens", usage.get("prompt_tokens", 0) + usage.get("completion_tokens", 0))
        h.wfile.write(_chunk({"id": cid, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage}))
        h.wfile.write(b"data: [DONE]\n\n")
        h.wfile.flush()
        if kind == "title":
            self.title_done_at = time.time()
            self.title_done.set()

    def close(self):
        self.stopping.set()
        self.server.shutdown()
        self.server.server_close()


def legacy_script(path):
    """The original smoke script: read `path`, then answer."""
    return [
        {"tools": [{"name": "read", "args": {"path": path}, "id": "call_1"}]},
        {"text": "The file says hi.", "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
    ]


if __name__ == "__main__":
    port = int(sys.argv[1])
    if len(sys.argv) > 2 and sys.argv[2] == "--script":
        script = json.load(open(sys.argv[3]))
    else:
        script = legacy_script(sys.argv[2]) * 1000
    m = Mock(port, script)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        m.close()
