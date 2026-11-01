"""Desktop browser attachment scenarios (2.2, `opencode.browser`): the `experimental.browser` RPC, its control events,
the session permissions an attachment grants, the `browser.*` Code Mode tools with their command/result round trip, file
transfers, timeouts, tunnels and detaching.

`Desktop` stands in for the desktop app's browser pane (packages/gui-plugins/src/browser/pane.ts): it follows its own
event stream, attaches once `server.connected` arrives, retrieves every announced command with `command` and answers it
with canned results, publishing its tab inventory with `state` before answering `tabs.open`/`tabs.close` like the real
pane. Tunnels are driven directly with the `tunnel.*` methods against loopback servers only.
"""
import base64
import http.client
import json
import socket
import threading
import time
import urllib.parse
import urllib.request
import uuid

from parity.registry import scenario

RPC = "/api/rpc/experimental.browser/"
CONTROL = "rpc.experimental.browser.control"
# A 1x1 PNG.
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==")


def tab_id(n):
    return f"tab_00000000-0000-4000-8000-{n:012d}"


def file_id(n):
    return f"file_00000000-0000-4000-9000-{n:012d}"


class Desktop:
    """One desktop attachment of a session. `answers` maps an action type to a function `(desktop, command)` that
    returns an outcome dict, or None to never answer."""

    def __init__(self, ctx, sid, connection=None, answers=None, version=4):
        self.ctx = ctx
        self.server = ctx.server
        self.sid = sid
        self.connection = connection or str(uuid.uuid4())
        self.answers = dict(DEFAULT_ANSWERS, **(answers or {}))
        self.version = version
        self.tabs = []
        self.focused = None
        self.commands = []
        self.cancels = []
        self.replies = []
        self.attached = threading.Event()
        self.done = threading.Event()
        self.outcome = None
        self.sock = None
        self._stream = None
        self._closed = False
        self.next_tab = 1

    # ---- transport -----------------------------------------------------------------------------------------------
    def path(self, method):
        return RPC + method + "?" + urllib.parse.urlencode({"directory": self.ctx.project})

    def call(self, method, input):
        status, _, data = self.server.request("POST", self.path(method), {"input": input})
        return status, data

    def attachment(self, **extra):
        return {"sessionID": self.sid, "connectionID": self.connection, **extra}

    def start(self, wait=True):
        connected = threading.Event()
        threading.Thread(target=self._events, args=(connected,), daemon=True).start()
        connected.wait(10)
        threading.Thread(target=self._attach, daemon=True).start()
        if wait:
            self.attached.wait(15)
            # The harness's own event stream has seen it too, so a mark taken now follows the `attached` event.
            self.ctx.wait_for(CONTROL, timeout=5, start=0, pred=lambda e: (e.get("data") or {}).get("connectionID")
                              == self.connection and (e.get("data") or {}).get("type") == "attached")
        return self

    def _attach(self):
        u = urllib.parse.urlparse(self.server.url)
        conn = http.client.HTTPConnection(u.hostname, u.port, timeout=600)
        body = json.dumps({"input": self.attachment(version=self.version)})
        try:
            conn.connect()
            self.sock = conn.sock
            conn.request("POST", self.path("attach"), body=body,
                         headers={"authorization": self.server.auth, "content-type": "application/json"})
            resp = conn.getresponse()
            raw = resp.read()
            try:
                data = json.loads(raw) if raw else None
            except Exception:
                data = raw.decode("utf-8", "replace")
            self.outcome = {"status": resp.status, "body": data}
        except Exception as e:  # detached by us
            self.outcome = {"detached": type(e).__name__ if not self._closed else True}
        finally:
            self.done.set()
            conn.close()

    def detach(self):
        """Drop the pending `attach` request, like a closed window or a lost network."""
        self._closed = True
        if self.sock:
            try:
                self.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.sock.close()
        self.done.wait(10)

    def stop(self):
        """Close the event stream (its socket: closing the response would wait for the reader's next line)."""
        self._closed = True
        if self._stream:
            try:
                self._stream.fp.raw._sock.shutdown(socket.SHUT_RDWR)
            except Exception:
                pass

    def wait(self, timeout=15):
        self.done.wait(timeout)
        return self.outcome

    def _events(self, connected):
        req = urllib.request.Request(self.server.url + "/api/event")
        req.add_header("authorization", self.server.auth)
        try:
            with urllib.request.urlopen(req) as resp:
                self._stream = resp
                for raw in resp:
                    line = raw.decode("utf-8", "replace").rstrip("\n")
                    if not line.startswith("data:"):
                        continue
                    event = json.loads(line[5:].strip())
                    if event.get("type") == "server.connected":
                        connected.set()
                        continue
                    if event.get("type") != CONTROL:
                        continue
                    data = event.get("data") or {}
                    if data.get("connectionID") != self.connection:
                        continue
                    if data.get("type") == "attached":
                        self.attached.set()
                    elif data.get("type") == "cancel":
                        self.cancels.append(data.get("requestID"))
                    elif data.get("type") == "command":
                        threading.Thread(target=self._command, args=(data["requestID"],), daemon=True).start()
        except Exception:
            connected.set()

    # ---- commands ------------------------------------------------------------------------------------------------
    def _command(self, request):
        status, data = self.call("command", self.attachment(requestID=request))
        if status >= 300:
            self.commands.append({"status": status, "body": data})
            return
        command = (data or {}).get("output")
        self.commands.append(command)
        answer = self.answers.get(command["action"]["type"])
        outcome = answer(self, command) if answer else failure("unsupported", "Not in this stand-in.")
        if outcome is None:
            return
        status, data = self.call("result", self.attachment(requestID=request, outcome=outcome))
        self.replies.append({"status": status, "body": data})

    def publish(self):
        """`state`: the authoritative tab inventory, acknowledged before results that depend on it."""
        return self.call("state", self.attachment(state=self.inventory()))

    def inventory(self):
        return {"tabs": [dict(t) for t in self.tabs], "focusedTabID": self.focused}

    def tab(self, tid):
        return next((t for t in self.tabs if t["id"] == tid), None)

    def open(self, url="about:blank", title=""):
        tab = {"id": tab_id(self.next_tab), "url": url, "title": title, "loading": False, "canGoBack": False,
               "canGoForward": False, "generation": 0}
        self.next_tab += 1
        self.tabs.append(tab)
        return tab


def success(value, files=()):
    return {"type": "success", "result": {"value": value, "files": list(files)}}


def failure(code, message):
    return {"type": "failure", "code": code, "message": message}


def _open(d, command):
    action = command["action"]
    tab = d.open(action.get("url", "about:blank"), "Example" if action.get("url") else "")
    if action.get("focus") is not False:
        d.focused = tab["id"]
    d.publish()
    return success(tab)


def _list(d, command):
    return success(d.inventory())


def _focus(d, command):
    d.focused = command["action"]["tabID"]
    d.publish()
    return success(d.tab(d.focused))


def _close(d, command):
    tid = command["action"]["tabID"]
    d.tabs = [t for t in d.tabs if t["id"] != tid]
    if d.focused == tid:
        d.focused = d.tabs[0]["id"] if d.tabs else None
    d.publish()
    return success(d.inventory())


def _navigate(d, command):
    tab = d.tab(command["action"]["tabID"])
    tab.update(url=command["action"]["url"], title="Navigated", canGoBack=True)
    d.publish()
    # Extra keys are not part of the output schema; the server drops them.
    return success(dict(tab, extra="dropped"))


def _snapshot(d, command):
    tab = d.tab(command["action"]["tabID"])
    return success({"truncated": False, "content": '- button "Sign in" [ref=e1]\n- link "Docs" [ref=e2]', "tab": tab,
                    "frames": "ignored"})


def _evaluate(d, command):
    tab = d.tab(command["action"]["tabID"])
    return success({"tab": tab, "value": {"title": "Example", "items": [1, 2.5, None, True], "nested": {"a": "b"}}})


def _screenshot(d, command):
    tab = d.tab(command["action"]["tabID"])
    return success({"tab": tab}, [{"id": file_id(1), "name": "screenshot.png", "mime": "image/png",
                                   "data": base64.b64encode(PNG).decode()}])


def _files_get(d, command):
    tab = d.tab(command["action"]["tabID"])
    return success({"tab": tab}, [{"id": command["action"]["fileID"], "name": "../CON.txt", "mime": "text/plain",
                                   "data": base64.b64encode(b"downloaded text").decode()}])


def _upload(d, command):
    return success(d.tab(command["action"]["tabID"]))


def _console(d, command):
    return failure("unsupported", "Console capture is not available in this desktop.")


def _network_list(d, command):
    # Missing `requests`: the plugin rejects it against the operation's output schema.
    return success({"tab": d.tab(command["action"]["tabID"]), "truncated": False, "dropped": 0})


def _hang(d, command):
    return None


def _preview(d, command):
    return success({"path": command["action"]["path"]})


DEFAULT_ANSWERS = {
    "tabs.list": _list,
    "tabs.open": _open,
    "tabs.focus": _focus,
    "tabs.close": _close,
    "navigate": _navigate,
    "snapshot": _snapshot,
    "evaluate": _evaluate,
    "screenshot": _screenshot,
    "files.get": _files_get,
    "files.upload": _upload,
    "files.drop": _upload,
    "console": _console,
    "network.list": _network_list,
    "wait": _hang,
    "preview": _preview,
}


# ------------------------------------------------------------------------------------------------------------------
# helpers
def execute(code, id="call_1"):
    return {"name": "execute", "args": {"code": code}, "id": id}


def turn(code, id="call_1"):
    return {"tools": [execute(code, id)]}


def session_permissions(ctx, sid, label):
    _, data = ctx.call(label, "GET", f"/api/session/{sid}", record=False)
    data = data.get("data", data) if isinstance(data, dict) else data
    ctx.rec(label, (data or {}).get("permissions"))


def agent_permissions(ctx, label):
    _, data = ctx.call(label, "GET", f"/api/agent?directory={urllib.parse.quote(ctx.project)}", record=False)
    agents = data.get("data", data) if isinstance(data, dict) else data
    build = next((a for a in agents or [] if a.get("id") == "build"), {})
    ctx.rec(label, (build.get("permissions") or [])[-3:])


def skills(ctx, label):
    _, data = ctx.call(label, "GET", f"/api/skill?directory={urllib.parse.quote(ctx.project)}", record=False)
    items = data.get("data", data) if isinstance(data, dict) else data
    ctx.rec(label, [s.get("id") for s in items or []])


def record_desktop(ctx, d, label="desktop"):
    ctx.rec(label + ".commands", d.commands)
    ctx.rec(label + ".replies", d.replies)
    ctx.rec(label + ".cancels", len(d.cancels))
    ctx.rec(label + ".outcome", d.outcome)


def browser_events(e):
    t = e.get("type", "")
    return t == CONTROL or t.startswith("session.permissions") or t == "session.updated"


def scrub(ctx):
    """Mask the random part of the server-local capture directory (`<tmp>/opencode-browser-XXXXXX/...`) and the random
    IDs of uploaded files (`file_<uuid>`: the normalizer's UUID mask needs a word boundary before the UUID)."""
    import re
    pattern = re.compile(r"opencode-browser-[A-Za-z0-9]{6}")
    upload = re.compile(r"file_(?!00000000-)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")

    def walk(v):
        if isinstance(v, str):
            return upload.sub("file_<upload>", pattern.sub("opencode-browser-XXXXXX", v))
        if isinstance(v, list):
            return [walk(x) for x in v]
        if isinstance(v, dict):
            return {k: walk(x) for k, x in v.items()}
        return v

    ctx.log = [(label, walk(value)) for label, value in ctx.log]


def tool_definitions(ctx, label, index=-1):
    """The tool definitions and the CodeMode catalog of one model request."""
    requests = ctx.mock.main_requests()
    if not requests:
        ctx.rec(label, None)
        return
    body = requests[index]["body"]
    ctx.rec(label + ".tools", body.get("tools"))
    system = [m for m in body.get("messages", []) if m.get("role") == "system"]
    ctx.rec(label + ".system", system)


# ------------------------------------------------------------------------------------------------------------------
@scenario("browser", timeout=240)
def browser_attach_lifecycle(ctx):
    """attach/state/command/result errors, the grant of the browser permissions and skill, replacement and detach."""
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Browser")
    agent_permissions(ctx, "agent.permissions")
    session_permissions(ctx, sid, "permissions.before")
    mark = len(ctx.server.events)
    first = Desktop(ctx, sid, connection="conn-first").start()
    ctx.rec("attached", first.attached.is_set())
    session_permissions(ctx, sid, "permissions.attached")
    skills(ctx, "skills.attached")
    ctx.rec("state", first.call("state", first.attachment(state={"tabs": [], "focusedTabID": None})))
    ctx.rec("state.other", first.call("state", {"sessionID": sid, "connectionID": "other",
                                                "state": {"tabs": [], "focusedTabID": None}}))
    ctx.rec("state.invalid", first.call("state", first.attachment(state={"tabs": [{"id": "nope"}], "focusedTabID": None})))
    ctx.rec("command.unknown", first.call("command", first.attachment(requestID="missing")))
    ctx.rec("result.unknown", first.call("result", first.attachment(requestID="missing",
                                                                     outcome=failure("x", "y"))))
    ctx.rec("result.other", first.call("result", {"sessionID": sid, "connectionID": "other", "requestID": "missing",
                                                  "outcome": failure("x", "y")}))
    ctx.rec("method.unknown", first.call("nope", first.attachment()))
    ctx.rec("attach.version", Desktop(ctx, sid, connection="conn-v3", version=3).call(
        "attach", {"sessionID": sid, "connectionID": "conn-v3", "version": 3}))
    ctx.rec("attach.no-session", first.call("attach", {"sessionID": "ses_missing", "connectionID": "x", "version": 4}))
    ctx.rec("attach.bad-session", first.call("attach", {"sessionID": "nope", "connectionID": "x", "version": 4}))
    ctx.rec("attach.no-input", ctx.server.request("POST", first.path("attach"), {})[::2])
    ctx.rec("attach.session-number", first.call("attach", {"sessionID": 1, "connectionID": "x", "version": 4}))
    ctx.rec("state.not-object", first.call("state", first.attachment(state=5)))
    ctx.rec("state.no-focus", first.call("state", first.attachment(state={"tabs": []})))
    ctx.rec("result.bad-outcome", first.call("result", first.attachment(requestID="r", outcome={"type": "nope"})))
    ctx.rec("result.bad-failure", first.call("result", first.attachment(requestID="r", outcome={"type": "failure"})))
    ctx.rec("result.bad-file-id", first.call("result", first.attachment(requestID="r", outcome=success(
        {}, [{"id": "file_1", "name": "a", "mime": "text/plain", "data": "AA=="}]))))
    ctx.rec("result.no-files", first.call("result", first.attachment(requestID="r", outcome={
        "type": "success", "result": {"value": 1}})))
    ctx.rec("tunnel.read.no-id", first.call("tunnel.read", first.attachment()))
    second = Desktop(ctx, sid, connection="conn-second").start()
    ctx.rec("first.outcome", first.wait())
    ctx.rec("second.attached", second.attached.is_set())
    session_permissions(ctx, sid, "permissions.replaced")
    second.detach()
    time.sleep(1.0)
    session_permissions(ctx, sid, "permissions.detached")
    skills(ctx, "skills.detached")
    ctx.rec("state.detached", second.call("state", second.attachment(state={"tabs": [], "focusedTabID": None})))
    first.stop()
    second.stop()
    ctx.event_start = mark
    ctx.record_events(filt=browser_events)


@scenario("browser", timeout=240)
def browser_tools(ctx):
    """the browser.* tools through Code Mode: tabs, navigation, snapshot, evaluate, captures and uploads."""
    ctx.make_project(files={"upload.txt": "hello upload\n"})
    ctx.set_script([
        turn("const tab = await tools.browser.tabs.open({ url: 'localhost:8000' })\n"
             "const snap = await tools.browser.snapshot({ tabID: tab.id })\n"
             "const value = await tools.browser.evaluate({ tabID: tab.id, script: 'document.title' })\n"
             "return { tab, snap, value }", "call_1"),
        turn("const { tabs } = await tools.browser.tabs.list({})\n"
             "const nav = await tools.browser.navigate({ tabID: tabs[0].id, url: 'example.com/docs' })\n"
             "await tools.browser.tabs.focus({ tabID: tabs[0].id })\n"
             "const shot = await tools.browser.screenshot({ tabID: tabs[0].id })\n"
             "return { nav, shot }", "call_2"),
        turn("const { tabs } = await tools.browser.tabs.list({})\n"
             "await tools.browser.files.upload({ tabID: tabs[0].id, ref: 'e1', paths: ['upload.txt'] })\n"
             f"const got = await tools.browser.files.get({{ tabID: tabs[0].id, fileID: '{file_id(7)}' }})\n"
             "const preview = await tools.browser.preview({ path: 'upload.txt' })\n"
             "const closed = await tools.browser.tabs.close({ tabID: tabs[0].id })\n"
             "return { got, preview, closed }", "call_3"),
        {"text": "Done."},
    ])
    ctx.start()
    sid = ctx.create_session(title="Browser tools")
    d = Desktop(ctx, sid, connection="conn-tools").start()
    ctx.prompt(sid, "Use the browser.")
    record_desktop(ctx, d)
    ctx.record_llm()
    tool_definitions(ctx, "request.first", 0)
    d.detach()
    d.stop()
    scrub(ctx)


@scenario("browser", timeout=240)
def browser_tool_errors(ctx):
    """tool failures: unknown tab, file paths as URLs, desktop failures, invalid results, input validation, uploads."""
    ctx.make_project(files={"dir/keep.txt": "x"})
    ctx.set_script([
        turn("const out = []\n"
             "const run = async (f) => { try { out.push(await f()) } catch (e) { out.push(String(e && e.message || e)) } }\n"
             f"await run(() => tools.browser.snapshot({{ tabID: '{tab_id(99)}' }}))\n"
             "const tab = await tools.browser.tabs.open({})\n"
             "await run(() => tools.browser.navigate({ tabID: tab.id, url: '/tmp/page.html' }))\n"
             "await run(() => tools.browser.navigate({ tabID: tab.id, url: 'file:///etc/hosts' }))\n"
             "await run(() => tools.browser.navigate({ tabID: tab.id, url: 'https://user:pw@example.com' }))\n"
             "await run(() => tools.browser.navigate({ tabID: tab.id, url: 'http://exa mple.com' }))\n"
             "await run(() => tools.browser.console({ tabID: tab.id }))\n"
             "await run(() => tools.browser.network.list({ tabID: tab.id }))\n"
             "await run(() => tools.browser.snapshot({ tabID: 'tab_1' }))\n"
             "await run(() => tools.browser.click({ tabID: tab.id }))\n"
             "await run(() => tools.browser.files.upload({ tabID: tab.id, ref: 'e1', paths: ['missing.txt'] }))\n"
             "await run(() => tools.browser.files.upload({ tabID: tab.id, ref: 'e1', paths: ['dir'] }))\n"
             "await run(() => tools.browser.files.upload({ tabID: tab.id, ref: 'e1', paths: [] }))\n"
             "await run(() => tools.browser.tabs.open({ url: 'https://example.com/' + 'é'.repeat(400) }))\n"
             "await run(() => tools.browser.tabs.open({ url: 'x'.repeat(2049) }))\n"
             "await run(() => tools.browser.console({ tabID: tab.id, limit: 501, level: 'verbose' }))\n"
             "await run(() => tools.browser.scroll({ tabID: tab.id, deltaY: 1.5 }))\n"
             "await run(() => tools.browser.evaluate({ tabID: tab.id, script: '1', extra: true }))\n"
             "return out", "call_1"),
        {"text": "Done."},
    ])
    ctx.start()
    sid = ctx.create_session(title="Browser errors")
    d = Desktop(ctx, sid, connection="conn-errors").start()
    ctx.prompt(sid, "Break the browser.")
    record_desktop(ctx, d)
    ctx.record_llm()
    d.detach()
    d.stop()


@scenario("browser", timeout=240)
def browser_disconnected(ctx):
    """a session allowed the browser without a desktop: the disconnected failure; a desktop that leaves mid-call."""
    ctx.make_project()
    ctx.set_script([
        turn("try { return await tools.browser.tabs.list({}) } catch (e) { return String(e.message) }", "call_1"),
        {"text": "Disconnected."},
        turn("try { return await tools.browser.tabs.open({ url: 'https://example.com' }) }"
             " catch (e) { return String(e.message) }", "call_2"),
        {"text": "Done."},
    ])
    ctx.start()
    sid = ctx.create_session(title="Browser disconnected")
    allow = [{"action": "browser", "resource": "*", "effect": "allow"},
             {"action": "skill", "resource": "browser", "effect": "allow"}]
    ctx.call("session.update", "PATCH", f"/api/session/{sid}", {"permissions": allow})
    ctx.prompt(sid, "List tabs.")
    d = Desktop(ctx, sid, connection="conn-leaves", answers={"tabs.open": _hang}).start()
    mark = len(ctx.server.events)
    ctx.call("session.prompt.2", "POST", f"/api/session/{sid}/prompt", {"text": "Open a tab."})
    deadline = time.time() + 20
    while not d.commands and time.time() < deadline:
        time.sleep(0.1)
    d.detach()
    ctx.wait_idle(sid, start=mark)
    session_permissions(ctx, sid, "permissions.after")
    record_desktop(ctx, d)
    ctx.record_llm()
    d.stop()


@scenario("browser", timeout=300)
def browser_timeout(ctx):
    """a command the desktop never answers fails after 60 seconds with a cancel control event."""
    ctx.make_project()
    ctx.set_script([
        turn("const tab = await tools.browser.tabs.open({})\n"
             "try { return await tools.browser.wait({ tabID: tab.id, condition: 'load' }) }"
             " catch (e) { return String(e.message) }", "call_1"),
        {"text": "Done."},
    ])
    ctx.start()
    sid = ctx.create_session(title="Browser timeout")
    d = Desktop(ctx, sid, connection="conn-timeout").start()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "Wait for it.", timeout=120)
    record_desktop(ctx, d)
    ctx.record_llm()
    ctx.event_start = mark
    ctx.record_events(filt=lambda e: e.get("type") == CONTROL)
    d.detach()
    d.stop()


def _echo_server():
    """A loopback TCP server that answers one HTTP request with a fixed response, then closes its write side."""
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(8)

    def serve():
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
            body = b"echo:" + data.split(b"\r\n", 1)[0]
            try:
                conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode()
                             + b"\r\nConnection: close\r\n\r\n" + body)
                conn.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            conn.close()

    threading.Thread(target=serve, daemon=True).start()
    return srv


# A loopback port nothing listens on, shared by both runs.
_CLOSED = socket.socket()
_CLOSED.bind(("127.0.0.1", 0))
CLOSED_PORT = _CLOSED.getsockname()[1]
_CLOSED.close()


@scenario("browser", timeout=240)
def browser_tunnel(ctx):
    """tunnel.open/read/write/close against a loopback server: bytes, half-close, eof and errors."""
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Browser tunnel")
    d = Desktop(ctx, sid, connection="conn-tunnel").start()
    srv = _echo_server()
    port = srv.getsockname()[1]
    try:
        status, data = d.call("tunnel.open", d.attachment(target={"host": "127.0.0.1", "port": port}))
        ctx.rec("open", {"status": status, "is_id": isinstance((data or {}).get("output"), str)})
        tid = (data or {}).get("output")
        request = b"GET /hello HTTP/1.1\r\nHost: example.test\r\n\r\n"
        ctx.rec("write", d.call("tunnel.write", d.attachment(tunnelID=tid, data=base64.b64encode(request).decode())))
        received, reads = b"", []
        for _ in range(20):
            status, data = d.call("tunnel.read", d.attachment(tunnelID=tid))
            out = (data or {}).get("output") if status < 300 else None
            if out is None:
                reads.append({"status": status, "body": data})
                break
            received += base64.b64decode(out["data"])
            if out["eof"]:
                reads.append({"eof": True, "data": out["data"]})
                break
        ctx.rec("received", received.decode())
        ctx.rec("reads.last", reads)
        ctx.rec("write.after-eof", d.call("tunnel.write", d.attachment(tunnelID=tid, data="", end=True)))
        ctx.rec("close", d.call("tunnel.close", d.attachment(tunnelID=tid)))
        ctx.rec("read.closed", d.call("tunnel.read", d.attachment(tunnelID=tid)))
        ctx.rec("write.closed", d.call("tunnel.write", d.attachment(tunnelID=tid, data="AA==")))
        ctx.rec("close.again", d.call("tunnel.close", d.attachment(tunnelID=tid)))
        ctx.rec("open.refused", d.call("tunnel.open", d.attachment(target={"host": "127.0.0.1", "port": CLOSED_PORT})))
        ctx.rec("open.refused.localhost", d.call("tunnel.open", d.attachment(
            target={"host": "localhost", "port": CLOSED_PORT})))
        ctx.rec("open.bad-port",d.call("tunnel.open", d.attachment(target={"host": "127.0.0.1", "port": 0})))
        ctx.rec("open.bad-host", d.call("tunnel.open", d.attachment(target={"host": "a b", "port": 80})))
        ctx.rec("open.other", d.call("tunnel.open", {"sessionID": sid, "connectionID": "other",
                                                     "target": {"host": "127.0.0.1", "port": port}}))
        # Half-close: end the write side with data, then read the answer.
        status, data = d.call("tunnel.open", d.attachment(target={"host": "localhost", "port": port}))
        tid = (data or {}).get("output")
        ctx.rec("write.end", d.call("tunnel.write", d.attachment(
            tunnelID=tid, data=base64.b64encode(b"POST /end HTTP/1.1\r\n\r\n").decode(), end=True)))
        received = b""
        for _ in range(20):
            status, data = d.call("tunnel.read", d.attachment(tunnelID=tid))
            out = (data or {}).get("output") if status < 300 else None
            if out is None or out["eof"]:
                break
            received += base64.b64decode(out["data"])
        ctx.rec("received.end", received.decode())
        # Detaching releases the attachment's sockets.
        status, data = d.call("tunnel.open", d.attachment(target={"host": "127.0.0.1", "port": port}))
        tid = (data or {}).get("output")
        d.detach()
        time.sleep(0.5)
        ctx.rec("read.detached", d.call("tunnel.read", d.attachment(tunnelID=tid)))
    finally:
        srv.close()
        d.stop()


@scenario("browser", timeout=240)
def browser_session_end(ctx):
    """deleting the attached session ends the attachment with `closed`; the disabled plugin answers rpc.unavailable."""
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Browser end")
    d = Desktop(ctx, sid, connection="conn-end").start()
    mark = len(ctx.server.events)
    ctx.call("session.delete", "DELETE", f"/api/session/{sid}")
    ctx.rec("outcome", d.wait())
    d.stop()
    ctx.event_start = mark
    ctx.record_events(filt=lambda e: e.get("type") == CONTROL)


@scenario("browser", timeout=240)
def browser_location_end(ctx):
    """an evicted location fails the pending attach with rpc.unavailable; a plugin reload before the browser plugin
    ends a later attachment with `closed`."""
    ctx.env_extra["OPENCODE_DISABLE_FILEWATCHER"] = "0"
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Browser location")
    d = Desktop(ctx, sid, connection="conn-evicted").start()
    ctx.call("debug.location.evict", "DELETE", f"/api/debug/location?directory={urllib.parse.quote(ctx.project)}")
    ctx.rec("evicted.outcome", d.wait())
    d.stop()
    time.sleep(1.0)
    session_permissions(ctx, sid, "permissions.evicted")
    d = Desktop(ctx, sid, connection="conn-reloaded").start()
    ctx.rec("reattached", d.attached.is_set())
    _configure(ctx, ["-opencode.goal"], "reload")
    ctx.rec("reloaded.outcome", d.wait(20))
    d.stop()
    time.sleep(1.0)
    session_permissions(ctx, sid, "permissions.reloaded")


def _configure(ctx, plugins, label):
    """Rewrite the project's `plugins` (file watching on) and wait for the reload."""
    path = f"{ctx.project}/opencode.json"
    cfg = json.load(open(path))
    cfg["plugins"] = plugins
    mark = len(ctx.server.events)
    with open(path, "w") as f:
        f.write(json.dumps(cfg, indent=2))
    ctx.rec(label + ".config", ctx.wait_for("config.updated", start=mark, timeout=15) is not None)


@scenario("browser", timeout=240)
def browser_disabled_live(ctx):
    """disabling the plugin while a desktop is attached ends the attachment with `closed`; the RPC is unavailable."""
    ctx.env_extra["OPENCODE_DISABLE_FILEWATCHER"] = "0"
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Browser disabled")
    d = Desktop(ctx, sid, connection="conn-disabled").start()
    ctx.rec("attached", d.attached.is_set())
    _configure(ctx, ["-opencode.browser"], "disable")
    ctx.rec("disabled.outcome", d.wait(20))
    d.stop()
    time.sleep(1.0)
    session_permissions(ctx, sid, "permissions.disabled")
    ctx.rec("state.disabled", d.call("state", d.attachment(state={"tabs": [], "focusedTabID": None})))


@scenario("browser", timeout=240)
def browser_disabled(ctx):
    """`-opencode.browser`: no deny rules, no browser skill, and the RPC is unavailable."""
    ctx.make_project(config_extra={"plugins": ["-opencode.browser"]})
    ctx.start()
    sid = ctx.create_session(title="Browser off")
    agent_permissions(ctx, "agent.permissions")
    skills(ctx, "skills")
    d = Desktop(ctx, sid, connection="conn-off")
    ctx.rec("attach", d.call("attach", d.attachment(version=4)))
    ctx.rec("state", d.call("state", d.attachment(state={"tabs": [], "focusedTabID": None})))
