"""2.3.1's experimental client-hosted MCP: a scripted client opens a WebSocket to `/api/experimental/client-mcp?name=`
and hosts an MCP server over it (one JSON-RPC message per frame, the server being the MCP client); the location's
sessions see its tools through Code Mode while the socket stays open and the model calls them with the calling
session in `_meta`. Also the ticket route and the socket's refusals and close codes."""
import json
import threading
import time
import urllib.parse

from parity.codemode230 import execute
from parity.registry import scenario
from parity.wsclient import Closed, Refused, WebSocket


class Client:
    """A client hosting an MCP server with `tools` (name -> answer text) over a socket to the server under test. It
    records each request it receives (method and params) and how the socket closed."""

    def __init__(self, ctx, name, tools, query=None, auth=True, binary=False, reject=False, ticket=None):
        q = {"location[directory]": ctx.project, "name": name, **(query or {})}
        if ticket:
            q["ticket"] = ticket
        url = ctx.server.url.replace("http://", "ws://") + "/api/experimental/client-mcp?" + urllib.parse.urlencode(q)
        self.tools, self.binary, self.reject = tools, binary, reject
        self.received, self.closed, self.refused = [], None, None
        self.attached = threading.Event()
        self.done = threading.Event()
        try:
            self.ws = WebSocket(url, headers={"authorization": ctx.server.auth} if auth else None)
        except Refused as e:
            self.ws, self.refused = None, {"status": e.status, "body": _body(e.body)}
            self.done.set()
            return
        threading.Thread(target=self._run, daemon=True).start()

    def _send(self, message):
        text = json.dumps(message)
        if self.binary:
            self.ws.send_binary(text.encode())
        else:
            self.ws.send(text)

    def _run(self):
        try:
            while True:
                message = json.loads(self.ws.recv())
                method, id_ = message.get("method"), message.get("id")
                self.received.append({"method": method, "params": message.get("params")} if method else message)
                if method == "initialize":
                    if self.reject:
                        self._send({"jsonrpc": "2.0", "id": id_, "error": {"code": -32603, "message": "not today"}})
                        continue
                    self._send({"jsonrpc": "2.0", "id": id_, "result": {
                        "protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}},
                        "serverInfo": {"name": "scripted-client", "version": "1.0.0"}}})
                elif method == "tools/list":
                    tools = [{"name": n, "description": f"Run {n}", "inputSchema": {"type": "object", "properties": {
                        "text": {"type": "string"}}}} for n in self.tools]
                    self._send({"jsonrpc": "2.0", "id": id_, "result": {"tools": tools}})
                    self.attached.set()
                elif method == "tools/call":
                    name = message["params"]["name"]
                    text = self.tools.get(name, f"no tool {name}")
                    args = message["params"].get("arguments") or {}
                    self._send({"jsonrpc": "2.0", "id": id_, "result": {
                        "content": [{"type": "text", "text": f"{text} {args.get('text', '')}".strip()}],
                        "isError": name not in self.tools}})
                elif id_ is not None and method:
                    self._send({"jsonrpc": "2.0", "id": id_, "result": {}})
        except Closed as c:
            self.closed = {"code": c.code, "reason": c.reason}
        except (OSError, ValueError) as e:
            self.closed = {"error": type(e).__name__}
        finally:
            self.done.set()

    def close(self):
        if self.ws:
            self.ws.close()
            self.done.wait(10)
            self.ws.shutdown()

    def methods(self):
        return [r.get("method") for r in self.received]

    def calls(self):
        return [r["params"] for r in self.received if r.get("method") == "tools/call"]


def _body(text):
    # An empty body may go out chunked (`0\r\n\r\n`) or with a zero length; both are empty.
    if text.strip() in ("", "0"):
        return ""
    try:
        return json.loads(text)
    except ValueError:
        return text


def _boot(ctx, script):
    ctx.set_script(script)
    ctx.make_project(config_extra={"permission": {"*": "allow"}})
    ctx.start()
    sid = ctx.create_session()
    return sid


def _settled(ctx, *clients, timeout=20):
    for c in clients:
        c.attached.wait(timeout)
    # The tools join the session's registry once their change settles (100 ms debounce).
    time.sleep(0.5)


@scenario("clientmcp231", timeout=120)
def clientmcp231_model_calls_client_tool(ctx):
    """A client named `tui` hosts `open_terminal` and `write_terminal`: the handshake opencode sends it (legacy
    `initialize`, its client info and capabilities), the attached server never listed by `/api/mcp`, the model calling
    both tools through Code Mode with the session in `_meta`, then the client disconnecting and the next request
    offering no `tui` tools."""
    sid = _boot(ctx, [
        {"tools": [execute('return await tools.tui.open_terminal({})', "call_open")]},
        {"tools": [execute('return await tools.tui.write_terminal({ text: "ls\\n" })', "call_write")]},
        {"tools": [execute('return await tools.tui.missing({})', "call_missing")]},
        {"text": "Terminal ready."},
        {"text": "No terminal."},
    ])
    client = Client(ctx, "tui", {"open_terminal": "Opened terminal pty_1", "write_terminal": "Wrote to terminal pty_1"})
    _settled(ctx, client)
    ctx.call("mcp.list", "GET", "/api/mcp")
    ctx.prompt(sid, "open a terminal and list the files", timeout=60)
    ctx.rec("client.received", client.received)
    client.close()
    ctx.rec("client.closed", client.closed)
    time.sleep(1)
    ctx.prompt(sid, "open it again", label="session.prompt.after", timeout=60)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("clientmcp231", timeout=120)
def clientmcp231_stacked_clients(ctx):
    """Two clients attach under one name: the newer one takes the calls while the older stays open, and once the newer
    disconnects the older one serves the name again (frames sent as binary are decoded as UTF-8)."""
    sid = _boot(ctx, [
        {"tools": [execute('return await tools.tui.open_terminal({})', "call_newer")]},
        {"text": "Newer."},
        {"tools": [execute('return await tools.tui.open_terminal({})', "call_older")]},
        {"text": "Older."},
    ])
    older = Client(ctx, "tui", {"open_terminal": "older client"})
    _settled(ctx, older)
    newer = Client(ctx, "tui", {"open_terminal": "newer client"}, binary=True)
    _settled(ctx, newer)
    ctx.prompt(sid, "open", timeout=60)
    newer.close()
    ctx.rec("newer.closed", newer.closed)
    time.sleep(1)
    ctx.prompt(sid, "open again", label="session.prompt.again", timeout=60)
    ctx.rec("older.calls", older.calls())
    ctx.rec("newer.calls", newer.calls())
    ctx.rec("older.open", older.closed is None)
    older.close()
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_llm()


@scenario("clientmcp231", hold_for_title=0, timeout=150)
def clientmcp231_tickets_and_refusals(ctx):
    """The ticket route (the `x-opencode-ticket: 1` header, the origin, the name pattern, the location envelope) and
    the socket's checks: no credentials (401), a ticket for another name or directory (403, the ticket still good for
    its own), a used ticket (403), an unknown ticket, no upgrade (400), a bad name (400), a missing location (404); then
    a client rejecting `initialize` (closed with 1011) and a location reload (closed with 1012, then attached again)."""
    _boot(ctx, [])
    import os
    other = os.path.join(ctx.project, "other")
    os.makedirs(other, exist_ok=True)

    def token(label, query, headers):
        status, ctype, data = ctx.server.request("POST", "/api/experimental/client-mcp/token?" + urllib.parse.urlencode(query),
                                                 None, headers={"x-opencode-directory": ctx.project, **headers})
        ticket = (data or {}).get("data", {}).get("ticket") if isinstance(data, dict) else None
        if ticket:
            data["data"]["ticket"] = f"<uuid of {len(ticket)} chars>"
        ctx.rec(label, {"status": status, "content-type": (ctype or "").split(";")[0], "body": data})

    ticket_header = {"x-opencode-ticket": "1"}
    token("token.no_header", {"name": "web"}, {})
    token("token.foreign_origin", {"name": "web"}, {**ticket_header, "origin": "https://evil.example"})
    token("token.bad_name", {"name": "bad name"}, ticket_header)
    token("token.no_name", {}, ticket_header)
    token("token.repeated_name", [("name", "web"), ("name", "x")], ticket_header)
    token("token.missing_location", {"name": "web", "location[directory]": os.path.join(ctx.project, "gone")}, ticket_header)
    token("token.other_location", {"name": "web", "location[directory]": other}, ticket_header)

    def fresh(name="web"):
        _, _, data = ctx.server.request("POST", "/api/experimental/client-mcp/token?name=" + name, None,
                                     headers={"x-opencode-directory": ctx.project, **ticket_header})
        return data["data"]["ticket"]

    def attempt(label, **kw):
        c = Client(ctx, kw.pop("name", "web"), {"probe": "ok"}, **kw)
        if c.refused is None:
            c.attached.wait(20)
            c.close()
        ctx.rec(label, c.refused or {"attached": c.attached.is_set(), "methods": c.methods()})

    attempt("ws.no_credentials", auth=False)
    ticket = fresh()
    attempt("ws.ticket_other_name", name="other", auth=False, ticket=ticket)
    attempt("ws.ticket_other_directory", auth=False, ticket=ticket, query={"location[directory]": other})
    attempt("ws.ticket", auth=False, ticket=ticket)
    attempt("ws.ticket_reused", auth=False, ticket=ticket)
    attempt("ws.ticket_unknown", auth=False, ticket="bogus")
    attempt("ws.bad_name", name="bad.name")
    attempt("ws.missing_location", query={"location[directory]": os.path.join(ctx.project, "gone")})

    def plain(label, query, headers=None):
        ctx.call(label, "GET", "/api/experimental/client-mcp?" + urllib.parse.urlencode(query),
                 headers={"x-opencode-directory": ctx.project, **(headers or {})})

    plain("http.no_upgrade", {"name": "web"})
    plain("http.bad_name", {"name": "bad name"})
    plain("http.foreign_origin", {"name": "web"}, {"origin": "https://evil.example"})
    plain("http.no_upgrade_missing_location", {"name": "web", "location[directory]": os.path.join(ctx.project, "gone")})

    rejecting = Client(ctx, "tui", {}, reject=True)
    rejecting.done.wait(20)
    ctx.rec("reject.closed", rejecting.closed)
    rejecting.ws.shutdown()

    attached = Client(ctx, "tui", {"open_terminal": "opened"})
    attached.attached.wait(20)
    ctx.call("location.reload", "POST", "/api/location/reload")
    attached.done.wait(20)
    ctx.rec("reload.closed", attached.closed)
    attached.ws.shutdown()
    again = Client(ctx, "tui", {"open_terminal": "opened"})
    ctx.rec("reload.attached_again", again.attached.wait(20))
    again.close()
    ctx.rec("again.closed", again.closed)
