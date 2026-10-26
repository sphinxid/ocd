"""Scriptable model-API mock: OpenAI chat completions, (Open) Responses over SSE and WebSocket, Anthropic Messages.

A script is a list of *turns*. Every non-title request consumes the first unconsumed turn whose optional `match`
substring occurs in the request body (turns without `match` match anything). Title-generation requests (system prompt
contains "title generator", or 2.1's "You name conversations") are answered with `title` and never consume a turn. When the script is exhausted the mock
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
  pace: float                seconds to wait between content chunks (chat completions; e.g. 0.005 = 200/s)
  hang: true                 never answer (until the client disconnects or the mock stops)
  match: str                 only consume this turn for requests whose JSON body contains the substring
  coalesce: true             chat completions: send the whole answer (content chunks, finish chunk, [DONE]) in one write,
                             so the client reads its frames together
  raw: [dict | str]          chat completions / Anthropic Messages: send exactly these SSE payloads (a dict as JSON; on
                             `.../messages` a dict with a `type` also gets its `event:` line), then end the response
                             (no finish chunk, no [DONE] unless listed); `stall` applies after the first

The wire protocol follows the request path: `.../responses` answers Open Responses / OpenAI Responses SSE events
(`response.created`, output items, `response.completed` with the output and usage), `.../messages` Anthropic Messages
SSE events, a `"stream": false` body a chat-completions JSON answer, anything else chat-completions chunks (`reasoning`,
`cut` and `stall` apply to chat completions, Responses and Anthropic Messages; `finish` sets the Anthropic
`stop_reason`). GET requests answer the JSON registered with
`Mock.routes[path] = (status, body)`, else 404. A GET with `Upgrade: websocket` opens a WebSocket on which every
`response.create` text frame consumes a turn the same way and is answered with the same Responses events as text
frames. Response IDs are `resp_<n>` per mock instance (`resp_t<n>` for title requests, so a title racing the first
step never shifts the step's IDs). Extra turn fields:
  error_frame: dict          WebSocket only: send this frame (e.g. an `error` event) instead of the response
  drop: true                 WebSocket only: send the content events, then drop the TCP connection (no terminal
                             event, no close frame); with no content (`text`/`tools`/`reasoning`) it drops at once
  close: [code, reason]      WebSocket only: like `drop`, but with a close frame
  reject_upgrade: int        the next WebSocket upgrade, when this is the first unused turn, is answered with this
                             HTTP status (requests and frames skip such turns)
"""
import base64
import hashlib
import json
import socket
import struct
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def _chunk(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode()


def decompress(raw, encoding):
    """Decode a `content-encoding` request body (gzip, or zstd through the `zstd` command)."""
    if encoding == "gzip":
        import gzip
        return gzip.decompress(raw)
    if encoding == "zstd":
        try:
            from compression import zstd  # Python 3.14+
            return zstd.decompress(raw)
        except ImportError:
            import subprocess
            return subprocess.run(["zstd", "-dc"], input=raw, capture_output=True, check=True).stdout
    return raw


def classify(body):
    text = json.dumps(body)
    if "title generator" in text or "You name conversations" in text:
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
                if (self.headers.get("upgrade") or "").lower() == "websocket":
                    try:
                        mock._websocket(self)
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        pass
                    self.close_connection = True
                    return
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
                # `encoding` (zstd/gzip) compresses request bodies; the recorded body is the decoded JSON and the
                # recorded headers keep `content-encoding`.
                raw = decompress(raw, (self.headers.get("content-encoding") or "").lower())
                try:
                    body = json.loads(raw)
                except Exception:
                    body = raw.decode("utf-8", "replace")
                kind = classify(body)
                turn = mock._take(kind, body)
                mock._record(kind, self.path, body, dict(self.headers))
                try:
                    if self.path.rstrip("/").endswith("/responses"):
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
            self.responses = 0
            self.title_responses = 0
            self.connections = 0

    def _record(self, kind, path, body, headers, **extra):
        with self.lock:
            self.requests.append({"kind": kind, "path": path, "body": body, "headers": headers, "at": time.time(),
                                  "transport": "http", **extra})

    def _take(self, kind, body):
        if kind == "title":
            return {"text": self.title}
        text = json.dumps(body)
        with self.lock:
            for i, turn in enumerate(self.script):
                if self.used[i] or turn.get("reject_upgrade"):
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
        if turn.get("raw") is not None:
            self._send_raw(h, turn, messages=False)
            return
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
        if turn.get("coalesce") and not turn.get("cut"):
            finish = turn.get("finish") or ("tool_calls" if turn.get("tools") else "stop")
            usage = dict(turn.get("usage") or {"prompt_tokens": 10, "completion_tokens": 5})
            usage.setdefault("total_tokens", usage.get("prompt_tokens", 0) + usage.get("completion_tokens", 0))
            out.append({"id": cid, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage})
            h.wfile.write(b"".join(_chunk(o) for o in out) + b"data: [DONE]\n\n")
            h.wfile.flush()
            return
        for i, o in enumerate(out):
            if i > 0 and turn.get("pace"):
                time.sleep(turn["pace"])
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

    def _send_raw(self, h, turn, messages):
        for i, item in enumerate(turn["raw"]):
            if isinstance(item, dict):
                line = "data: " + json.dumps(item) + "\n\n"
                if messages and item.get("type"):
                    line = "event: " + item["type"] + "\n" + line
            else:
                line = "data: " + item + "\n\n"
            h.wfile.write(line.encode())
            h.wfile.flush()
            if i == 0 and turn.get("stall"):
                time.sleep(turn["stall"])
        h.wfile.flush()
        try:
            h.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

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

    # ---- Open Responses / OpenAI Responses (HTTP SSE + WebSocket) -------------------------------------------
    def _next_response_id(self, kind):
        """`resp_<n>` for model steps, `resp_t<n>` for titles (separate counters: titles race the first step)."""
        with self.lock:
            if kind == "title":
                self.title_responses += 1
                return f"resp_t{self.title_responses}"
            self.responses += 1
            return f"resp_{self.responses}"

    def responses_events(self, turn, kind, model="mock"):
        """The Responses events answering one turn (without the terminal event when `drop`/`close`/`cut` is set)."""
        rid = self._next_response_id(kind)
        n = rid.split("_")[1]
        created = {"type": "response.created",
                   "response": {"id": rid, "object": "response", "created_at": 0, "model": model,
                                "status": "in_progress", "output": []}}
        events = [created]
        output = []

        def added(index, value):
            events.append({"type": "response.output_item.added", "output_index": index,
                           "item": {**value, "status": "in_progress"}})

        reasoning = turn.get("reasoning") if kind != "title" else None
        if reasoning:
            parts = [reasoning] if isinstance(reasoning, str) else list(reasoning)
            iid = f"rs_{n}_{len(output)}"
            added(len(output), {"id": iid, "type": "reasoning", "summary": []})
            for p in parts:
                events.append({"type": "response.reasoning_summary_text.delta", "item_id": iid,
                               "output_index": len(output), "summary_index": 0, "delta": p})
            done = {"id": iid, "type": "reasoning", "summary": [{"type": "summary_text", "text": "".join(parts)}],
                    "encrypted_content": f"enc_{n}"}
            events.append({"type": "response.output_item.done", "output_index": len(output), "item": done})
            output.append(done)
        text = turn.get("text")
        if text:
            parts = [text] if isinstance(text, str) else list(text)
            iid = f"msg_{n}_{len(output)}"
            added(len(output), {"id": iid, "type": "message", "role": "assistant", "content": []})
            for p in parts:
                events.append({"type": "response.output_text.delta", "item_id": iid, "output_index": len(output),
                               "content_index": 0, "delta": p})
            done = {"id": iid, "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "".join(parts), "annotations": []}]}
            events.append({"type": "response.output_item.done", "output_index": len(output), "item": done})
            output.append(done)
        for i, tool in enumerate((turn.get("tools") or []) if kind != "title" else []):
            args = tool.get("args", {})
            args = args if isinstance(args, str) else json.dumps(args)
            iid = f"fc_{n}_{len(output)}"
            call = {"id": iid, "type": "function_call", "call_id": tool.get("id", f"call_{i + 1}"),
                    "name": tool["name"]}
            added(len(output), {**call, "arguments": ""})
            events.append({"type": "response.function_call_arguments.delta", "item_id": iid,
                           "output_index": len(output), "delta": args})
            events.append({"type": "response.function_call_arguments.done", "item_id": iid,
                           "output_index": len(output), "arguments": args})
            done = {**call, "arguments": args, "status": "completed"}
            events.append({"type": "response.output_item.done", "output_index": len(output), "item": done})
            output.append(done)
        if kind != "title" and (turn.get("drop") or turn.get("close") or turn.get("cut")):
            return events
        usage = turn.get("usage") or {"prompt_tokens": 10, "completion_tokens": 5}
        usage = {"input_tokens": usage.get("prompt_tokens", 10), "output_tokens": usage.get("completion_tokens", 5),
                 "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}
        usage["total_tokens"] = usage["input_tokens"] + usage["output_tokens"]
        events.append({"type": "response.completed",
                       "response": {"id": rid, "object": "response", "created_at": 0, "model": model,
                                    "status": "completed", "output": output, "usage": usage}})
        return events

    def _answer_responses(self, h, kind, turn, body):
        """Responses over HTTP: the turn's events as an SSE stream (`cut` drops the connection before the end)."""
        if self._prelude(h, kind, turn):
            return
        model = body.get("model", "mock") if isinstance(body, dict) else "mock"
        for i, event in enumerate(self.responses_events(turn, kind, model)):
            h.wfile.write(("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n").encode())
            h.wfile.flush()
            if i == 1 and turn.get("stall"):
                time.sleep(turn["stall"])
        if turn.get("cut") and kind != "title":
            try:
                h.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        self._finish(kind)

    def _answer_messages(self, h, kind, turn, body):
        """Anthropic Messages API stream: text block, tool_use blocks, message_delta with the stop reason."""
        if self._prelude(h, kind, turn):
            return
        if turn.get("raw") is not None:
            self._send_raw(h, turn, messages=True)
            return
        model = body.get("model", "mock") if isinstance(body, dict) else "mock"
        usage = turn.get("usage") or {}
        sent = [0]

        def send(obj):
            h.wfile.write(("event: " + obj["type"] + "\ndata: " + json.dumps(obj) + "\n\n").encode())
            h.wfile.flush()
            sent[0] += 1
            if sent[0] == 2 and turn.get("stall"):
                time.sleep(turn["stall"])

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
        if turn.get("cut"):
            try:
                h.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        stop = turn.get("finish") or ("tool_use" if turn.get("tools") else "end_turn")
        send({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
              "usage": {"output_tokens": usage.get("completion_tokens", 5)}})
        send({"type": "message_stop"})
        self._finish(kind)

    def _websocket(self, h):
        headers = dict(h.headers)
        with self.lock:
            self.connections += 1
            conn = self.connections
            rejected = None
            # An upgrade is refused when the next unused turn says so.
            for i, turn in enumerate(self.script):
                if self.used[i]:
                    continue
                if turn.get("reject_upgrade"):
                    self.used[i] = True
                    rejected = turn["reject_upgrade"]
                break
        self._record("ws.connect", h.path, None, headers, transport="websocket", conn=conn,
                     **({"rejected": rejected} if rejected else {}))
        if rejected:
            payload = json.dumps({"error": {"message": "upgrade rejected", "type": "invalid_request_error"}}).encode()
            h.send_response(rejected)
            h.send_header("content-type", "application/json")
            h.send_header("content-length", str(len(payload)))
            h.end_headers()
            h.wfile.write(payload)
            return
        key = h.headers.get("sec-websocket-key", "")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        # The upgrade answer must be HTTP/1.1 (the handler's default status line is HTTP/1.0).
        h.protocol_version = "HTTP/1.1"
        h.send_response(101, "Switching Protocols")
        h.send_header("Upgrade", "websocket")
        h.send_header("Connection", "Upgrade")
        h.send_header("Sec-WebSocket-Accept", accept.decode())
        h.end_headers()
        h.wfile.flush()
        sock = h.connection
        rfile = h.rfile
        while not self.stopping.is_set():
            frame = _ws_read(rfile)
            if frame is None:
                self._record("ws.eof", h.path, None, {}, transport="websocket", conn=conn)
                return
            opcode, data = frame
            if opcode == 8:
                code = struct.unpack(">H", data[:2])[0] if len(data) >= 2 else None
                self._record("ws.close", h.path, {"code": code}, {}, transport="websocket", conn=conn)
                try:
                    _ws_send(sock, 8, data[:2])
                except OSError:
                    pass
                return
            if opcode == 9:
                _ws_send(sock, 10, data)
                continue
            if opcode != 1:
                continue
            text = data.decode("utf-8", "replace")
            try:
                body = json.loads(text)
            except Exception:
                body = text
            kind = classify(body)
            turn = self._take(kind, body)
            self._record(kind, h.path, body, {}, transport="websocket", conn=conn, raw=text)
            if kind == "main" and self.hold_for_title > 0 and self.title_done.wait(self.hold_for_title):
                time.sleep(max(0.0, self.title_done_at + 0.5 - time.time()))
            if turn.get("delay"):
                time.sleep(turn["delay"])
            if turn.get("hang"):
                continue
            if turn.get("error_frame"):
                _ws_send(sock, 1, json.dumps(turn["error_frame"]).encode())
                continue
            model = body.get("model", "mock") if isinstance(body, dict) else "mock"
            events = self.responses_events(turn, kind, model)
            if (turn.get("drop") or turn.get("close")) and len(events) == 1:
                events = []
            for i, event in enumerate(events):
                _ws_send(sock, 1, json.dumps(event).encode())
                if i == 1 and turn.get("stall"):
                    time.sleep(turn["stall"])
            if turn.get("close"):
                code, reason = turn["close"]
                _ws_send(sock, 8, struct.pack(">H", code) + reason.encode())
                time.sleep(0.2)
            if turn.get("drop") or turn.get("close"):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                self._record("ws.dropped", h.path, None, {}, transport="websocket", conn=conn)
                return

    def close(self):
        self.stopping.set()
        self.server.shutdown()
        self.server.server_close()


def _ws_read(rfile):
    """One complete WebSocket message from a client (masked frames, continuation frames joined)."""
    payload = b""
    first = None
    while True:
        head = rfile.read(2)
        if len(head) < 2:
            return None
        fin, opcode = head[0] & 0x80, head[0] & 0x0F
        masked, length = head[1] & 0x80, head[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", rfile.read(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", rfile.read(8))[0]
        mask = rfile.read(4) if masked else b"\0\0\0\0"
        data = bytearray(rfile.read(length))
        for i in range(len(data)):
            data[i] ^= mask[i % 4]
        if opcode >= 8:
            return opcode, bytes(data)
        if first is None:
            first = opcode
        payload += bytes(data)
        if fin:
            return first, payload


def _ws_send(sock, opcode, data):
    head = bytes([0x80 | opcode])
    n = len(data)
    if n < 126:
        head += bytes([n])
    elif n < 65536:
        head += bytes([126]) + struct.pack(">H", n)
    else:
        head += bytes([127]) + struct.pack(">Q", n)
    sock.sendall(head + data)


def legacy_script(path):
    """The original smoke script: read `path`, then answer."""
    return [
        {"tools": [{"name": "read", "args": {"path": path}, "id": "call_1"}]},
        {"text": "The file says hi.", "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
    ]
