"""Responses WebSocket transport: a configured provider on the native OpenAI package with `transport: "websocket"`
talks to the mock's Open Responses endpoint (scripts/parity/mock.py). Steps of a session go over one socket per
session (continuation frames with `previous_response_id`), titles over HTTP; errors, dropped sockets and a refused
upgrade (HTTP fallback) are compared, with every frame each binary sent."""
import hashlib
import json
import time

from parity.plugins import BUN, plugin_files
from parity.registry import scenario

# Handshake headers that differ by WebSocket client and carry no request data: the random key, the host
# with the mock's port, and Bun's `permessage-deflate` offer (ocd's client offers no extensions).
WS_HIDDEN = {"host", "sec-websocket-key", "sec-websocket-extensions"}
# HTTP client details (Bun's fetch vs reqwest), as in `record_llm`.
HTTP_HIDDEN = {"host", "connection", "content-length", "accept-encoding"}


def config(ctx, extra=None):
    cfg = {"model": "mockws/m1", "providers": {"mockws": {
        "name": "Mock WS", "package": "@opencode/ai/providers/openai",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test", "transport": "websocket"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
    cfg.update(extra or {})
    return json.dumps(cfg, indent=2)


def digest(text):
    return f"sha256:{hashlib.sha256(text.encode()).hexdigest()[:16]} ({len(text)} chars)"


def shrink(body):
    """A request body with the long, already-compared parts (instructions, tool schemas) reduced to digests."""
    if not isinstance(body, dict):
        return body
    out = dict(body)
    if isinstance(out.get("instructions"), str):
        out["instructions"] = digest(out["instructions"])
    if isinstance(out.get("tools"), list):
        out["tools"] = [t.get("name") for t in out["tools"]] + [digest(json.dumps(out["tools"], sort_keys=True))]
    return out


def key_order(value):
    """The key order of a JSON frame (top level, and the first-level objects inside `input`)."""
    if not isinstance(value, dict):
        return None
    out = {"top": list(value.keys())}
    if isinstance(value.get("input"), list):
        out["input"] = [list(i.keys()) for i in value["input"] if isinstance(i, dict)]
    return out


def record_wire(ctx, label="wire"):
    """Everything each binary sent to the mock: socket upgrades (headers), frames (body and key order, per
    connection), socket closes, and HTTP requests (title requests listed apart, they race the first step)."""
    titles, main = [], []
    for r in ctx.mock.requests:
        hidden = WS_HIDDEN if r.get("transport") == "websocket" else HTTP_HIDDEN
        entry = {"kind": r["kind"], "transport": r.get("transport"), "path": r["path"]}
        for key in ("conn", "rejected"):
            if r.get(key):
                entry[key] = r[key]
        if r["headers"]:
            entry["headers"] = {k.lower(): v for k, v in r["headers"].items() if k.lower() not in hidden}
        if r["body"] is not None:
            entry["body"] = shrink(r["body"])
        if r.get("raw"):
            entry["keys"] = key_order(json.loads(r["raw"]))
        (titles if r["kind"] == "title" else main).append(entry)
    ctx.rec(label + ".title", titles)
    ctx.rec(label, main)


def finish(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()
    # Stopping the server closes the session sockets (normal closure) before the wire is recorded.
    ctx.stop()
    record_wire(ctx)


@scenario("websocket")
def ws_session(ctx):
    """Two prompts over one socket: a read tool round trip and an answer as continuation frames, then a
    `previous_response_not_found` rejection retried in full on a new socket, a server error frame (retry on a new
    socket), a dropped connection mid-answer (continue) and a close frame before any output (retry)."""
    ctx.set_script([
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}, "id": "call_1"}]},
        {"text": "The file says hi.", "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
        {"error_frame": {"type": "error", "status": 400, "error": {
            "type": "invalid_request_error", "code": "previous_response_not_found",
            "message": "Previous response with id 'resp_2' not found.", "param": "previous_response_id"}}},
        {"reasoning": ["Thinking ", "again."], "text": ["Second ", "answer."]},
        {"error_frame": {"type": "error", "error": {
            "type": "server_error", "code": "server_error",
            "message": "The server had an error processing your request."}}},
        {"text": "Third answer."},
        {"text": ["Partial ", "answer"], "drop": True},
        {"text": "Fourth answer."},
        {"close": [1011, "internal error"]},
        {"text": "Fifth answer."},
    ])
    ctx.make_project(files={"hello.txt": "hi there\n", "opencode.json": config(ctx)})
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "what is in hello.txt?")
    ctx.prompt(sid, "and again?")
    ctx.prompt(sid, "third")
    ctx.prompt(sid, "fourth", timeout=90)
    ctx.prompt(sid, "fifth", timeout=90)
    finish(ctx, sid)


@scenario("websocket", hold_for_title=0)
def ws_interrupt(ctx):
    """An interrupt while the socket waits for an answer closes it; the next prompt opens a new socket and sends
    the whole conversation."""
    ctx.set_script([{"text": "One."}, {"hang": True}, {"text": "After interrupt."}])
    ctx.make_project(files={"opencode.json": config(ctx)})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "first")
    mark = len(ctx.server.events)
    ctx.prompt(sid, "hang please", wait=False)
    deadline = time.time() + 20
    while time.time() < deadline and len(ctx.mock.main_requests()) < 2:
        time.sleep(0.1)
    time.sleep(0.5)
    ctx.call("interrupt", "POST", f"/api/session/{sid}/interrupt")
    ctx.wait_idle(sid, start=mark)
    ctx.prompt(sid, "again")
    finish(ctx, sid)


@scenario("websocket", hold_for_title=0)
def ws_fallback(ctx):
    """A refused upgrade pins the session to HTTP: both steps go to the HTTP endpoint (with tracing headers); a
    second session still gets its own socket; a provider with `transport: "http"` never opens one."""
    ctx.set_script([
        {"reject_upgrade": 403},
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}, "id": "call_1"}]},
        {"text": "Read over HTTP."},
        {"text": "Second session over the socket."},
        {"text": "Third session over plain HTTP."},
    ])
    http = {"providers": {"mockhttp": {
        "name": "Mock HTTP", "package": "@opencode/ai/providers/openai",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test", "transport": "http"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
    cfg = json.loads(config(ctx))
    cfg["providers"].update(http["providers"])
    ctx.make_project(files={"hello.txt": "hi there\n", "opencode.json": json.dumps(cfg, indent=2)})
    ctx.start()
    sid = ctx.create_session(title="First")
    ctx.prompt(sid, "what is in hello.txt?")
    other = ctx.create_session(title="Second")
    ctx.prompt(other, "hello")
    third = ctx.create_session(title="Third", model={"providerID": "mockhttp", "id": "m1"})
    ctx.prompt(third, "over http")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.stop()
    record_wire(ctx)


@scenario("websocket")
def ws_plugin_hooks(ctx):
    """`experimental.ws.handshake/send/receive` plugin hooks rewrite the upgrade (URL + header), every sent frame and
    the received text; `session.http.request` sees only HTTP requests (the title and, after an error frame and a
    refused reconnect, the HTTP fallback of the retried step)."""
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script([
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}, "id": "call_1"}]},
        {"text": ["Say ORIGINAL", " now"]},
        {"error_frame": {"type": "error", "error": {"type": "server_error", "message": "busy"}}},
        {"reject_upgrade": 403},
        {"text": "ORIGINAL over HTTP."},
    ])
    files = dict(plugin_files("ws-plugin"), **{"hello.txt": "hi\n",
                 "opencode.json": config(ctx, {"plugins": ["./plugins/ws-plugin"]})})
    ctx.make_project(files=files)
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "read hello")
    ctx.prompt(sid, "again", timeout=90)
    finish(ctx, sid)
