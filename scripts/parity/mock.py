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
  stall: float               seconds to pause after the first content chunk (the step is visibly mid-stream)
  hang: true                 never answer (until the client disconnects or the mock stops)
  match: str                 only consume this turn for requests whose JSON body contains the substring

The wire protocol follows the request path: `.../responses` answers OpenAI Responses SSE events, `.../messages`
Anthropic Messages SSE events, anything else chat-completions chunks (reasoning, `cut` and `stall` apply to chat
completions only). GET requests answer the JSON registered with `Mock.routes[path] = (status, body)`, else 404.
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
        # GET path -> (status, JSON body)
        self.routes = {}
        self.set_script(script or [])
        mock = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                mock._record("GET", self.path, None, dict(self.headers))
                route = mock.routes.get(self.path)
                if route is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                payload = json.dumps(route[1]).encode()
                self.send_response(route[0])
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

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
                    if self.path.endswith("/responses"):
                        mock._answer_responses(self, kind, turn, body)
                    elif self.path.endswith("/messages"):
                        mock._answer_messages(self, kind, turn, body)
                    elif isinstance(body, dict) and body.get("stream") is False:
                        mock._answer_json(self, kind, turn, body)
                    else:
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
        for i, o in enumerate(out):
            h.wfile.write(_chunk(o))
            h.wfile.flush()
            if i == 0 and turn.get("stall"):
                time.sleep(turn["stall"])
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

    def _prelude(self, h, kind, turn):
        """Shared start of an answer: title hold, delay, hang and HTTP errors. True when the answer is complete."""
        if kind == "main" and self.hold_for_title > 0 and self.title_done.wait(self.hold_for_title):
            time.sleep(max(0.0, self.title_done_at + 0.5 - time.time()))
        if turn.get("delay"):
            time.sleep(turn["delay"])
        if turn.get("hang"):
            while not self.stopping.is_set():
                time.sleep(0.05)
            return True
        if turn.get("status"):
            payload = json.dumps(turn.get("body", {"error": {"message": "mock error", "type": "server_error"}})).encode()
            h.send_response(turn["status"])
            h.send_header("content-type", "application/json")
            for k, v in (turn.get("headers") or {}).items():
                h.send_header(k, str(v))
            h.send_header("content-length", str(len(payload)))
            h.end_headers()
            h.wfile.write(payload)
            return True
        h.send_response(200)
        h.send_header("content-type", "text/event-stream")
        h.end_headers()
        return False

    def _finish(self, kind):
        if kind == "title":
            self.title_done_at = time.time()
            self.title_done.set()

    def _answer_json(self, h, kind, turn, body):
        """A non-streaming chat completion (`"stream": false`): the turn's text as one message."""
        if kind == "main" and self.hold_for_title > 0 and self.title_done.wait(self.hold_for_title):
            time.sleep(max(0.0, self.title_done_at + 0.5 - time.time()))
        text = turn.get("text")
        text = "".join(text) if isinstance(text, list) else (text or "")
        if turn.get("status"):
            self._prelude(h, kind, turn)
            return
        usage = dict(turn.get("usage") or {"prompt_tokens": 10, "completion_tokens": 5})
        usage.setdefault("total_tokens", usage.get("prompt_tokens", 0) + usage.get("completion_tokens", 0))
        payload = json.dumps({"id": "chatcmpl-mock", "object": "chat.completion", "created": 0,
                              "model": body.get("model", "mock"),
                              "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                           "finish_reason": "stop"}], "usage": usage}).encode()
        h.send_response(200)
        h.send_header("content-type", "application/json")
        h.send_header("content-length", str(len(payload)))
        h.end_headers()
        h.wfile.write(payload)
        self._finish(kind)

    def _answer_responses(self, h, kind, turn, body):
        """OpenAI Responses API stream: a message item with the text deltas, function_call items, completion."""
        if self._prelude(h, kind, turn):
            return
        model = body.get("model", "mock") if isinstance(body, dict) else "mock"

        def send(obj):
            h.wfile.write(("event: " + obj["type"] + "\ndata: " + json.dumps(obj) + "\n\n").encode())
            h.wfile.flush()

        send({"type": "response.created", "response": {"id": "resp_mock", "created_at": 0, "model": model}})
        index = 0
        text = turn.get("text")
        pieces = [text] if isinstance(text, str) else (text or [])
        if pieces:
            send({"type": "response.output_item.added", "output_index": index,
                  "item": {"type": "message", "id": "msg_mock", "role": "assistant", "content": []}})
            for piece in pieces:
                send({"type": "response.output_text.delta", "item_id": "msg_mock", "output_index": index,
                      "content_index": 0, "delta": piece})
            send({"type": "response.output_item.done", "output_index": index,
                  "item": {"type": "message", "id": "msg_mock", "role": "assistant",
                           "content": [{"type": "output_text", "text": "".join(pieces), "annotations": []}]}})
            index += 1
        for i, tool in enumerate(turn.get("tools") or []):
            args = tool.get("args", {})
            args = args if isinstance(args, str) else json.dumps(args)
            call_id = tool.get("id", f"call_{i + 1}")
            item = {"type": "function_call", "id": f"fc_{i + 1}", "call_id": call_id, "name": tool["name"],
                    "arguments": ""}
            send({"type": "response.output_item.added", "output_index": index, "item": item})
            send({"type": "response.function_call_arguments.delta", "item_id": item["id"], "output_index": index,
                  "delta": args})
            send({"type": "response.output_item.done", "output_index": index,
                  "item": dict(item, arguments=args, status="completed")})
            index += 1
        usage = turn.get("usage") or {}
        send({"type": "response.completed", "response": {
            "id": "resp_mock", "created_at": 0, "model": model, "status": "completed",
            "usage": {"input_tokens": usage.get("prompt_tokens", 10), "output_tokens": usage.get("completion_tokens", 5),
                      "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}})
        self._finish(kind)

    def _answer_messages(self, h, kind, turn, body):
        """Anthropic Messages API stream: text block, tool_use blocks, message_delta with the stop reason."""
        if self._prelude(h, kind, turn):
            return
        model = body.get("model", "mock") if isinstance(body, dict) else "mock"
        usage = turn.get("usage") or {}

        def send(obj):
            h.wfile.write(("event: " + obj["type"] + "\ndata: " + json.dumps(obj) + "\n\n").encode())
            h.wfile.flush()

        send({"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": usage.get("prompt_tokens", 10), "output_tokens": 1}}})
        index = 0
        text = turn.get("text")
        pieces = [text] if isinstance(text, str) else (text or [])
        if pieces:
            send({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}})
            for piece in pieces:
                send({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": piece}})
            send({"type": "content_block_stop", "index": index})
            index += 1
        for i, tool in enumerate(turn.get("tools") or []):
            args = tool.get("args", {})
            args = args if isinstance(args, str) else json.dumps(args)
            send({"type": "content_block_start", "index": index, "content_block": {
                "type": "tool_use", "id": tool.get("id", f"toolu_{i + 1}"), "name": tool["name"], "input": {}}})
            send({"type": "content_block_delta", "index": index,
                  "delta": {"type": "input_json_delta", "partial_json": args}})
            send({"type": "content_block_stop", "index": index})
            index += 1
        stop = "tool_use" if turn.get("tools") else "end_turn"
        send({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
              "usage": {"output_tokens": usage.get("completion_tokens", 5)}})
        send({"type": "message_stop"})
        self._finish(kind)

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
