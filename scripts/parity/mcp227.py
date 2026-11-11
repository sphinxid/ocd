"""2.2.7 MCP OAuth: a refresh under the shared credential lock rotates the refresh token once across Locations
(#62827), a temporary refresh failure keeps the login (#63012) while a rejected refresh token still signs out, and
the sign-in accepts the redirect URL pasted from the browser (#61706).

The remote MCP server and its authorization server are one local fake on 127.0.0.1 (`FakeRemote`), on the same port
for both binaries so the MCP integration ID (`mcp_` + sha1(name, URL)) is the same."""
import hashlib
import json
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from parity.registry import scenario

NAME = "secure"


class FakeRemote:
    """A legacy Streamable HTTP MCP server that wants a bearer token, plus its OAuth authorization server."""

    def __init__(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def send(self, status, body=None, headers=None):
                data = b"" if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
                self.send_response(status)
                if body is not None and not isinstance(body, bytes):
                    self.send_header("content-type", "application/json")
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def body(self):
                n = int(self.headers.get("content-length") or 0)
                return self.rfile.read(n) if n else b""

            def do_GET(self):
                base = fake.base
                path = self.path.split("?")[0]
                if path in ("/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource"):
                    return self.send(200, {"resource": f"{base}/mcp", "authorization_servers": [base]})
                if path == "/.well-known/oauth-authorization-server":
                    return self.send(200, {
                        "issuer": base, "authorization_endpoint": f"{base}/authorize", "token_endpoint": f"{base}/token",
                        "registration_endpoint": f"{base}/register", "response_types_supported": ["code"],
                        "code_challenge_methods_supported": ["S256"], "token_endpoint_auth_methods_supported": ["none"],
                        "grant_types_supported": ["authorization_code", "refresh_token"]})
                if path == "/mcp":
                    return self.send(405)
                return self.send(404, b"not found")

            def do_DELETE(self):
                return self.send(200, b"")

            def do_POST(self):
                path = self.path.split("?")[0]
                raw = self.body()
                if path == "/register":
                    client = json.loads(raw)
                    with fake.lock:
                        fake.registrations.append(client)
                    client["client_id"] = "dyn-client"
                    return self.send(201, client)
                if path == "/token":
                    return self.token(dict(urllib.parse.parse_qsl(raw.decode())))
                if path != "/mcp":
                    return self.send(404, b"not found")
                auth = self.headers.get("authorization") or ""
                with fake.lock:
                    fake.bearers.append(auth)
                    ok = auth.removeprefix("Bearer ") in fake.valid
                if not ok:
                    return self.send(401, b"unauthorized", {
                        "www-authenticate": f'Bearer resource_metadata="{fake.base}/.well-known/oauth-protected-resource/mcp"'})
                msg = json.loads(raw)
                if "id" not in msg:
                    return self.send(202)
                method, mid = msg.get("method"), msg["id"]
                if method == "initialize":
                    result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
                              "serverInfo": {"name": "fake-remote", "version": "1.0.0"}}
                elif method == "tools/list":
                    result = {"tools": [{"name": "hello", "description": "Says hello.", "inputSchema": {"type": "object"}}]}
                elif method == "ping":
                    result = {}
                else:
                    return self.send(200, {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
                return self.send(200, {"jsonrpc": "2.0", "id": mid, "result": result})

            def token(self, form):
                grant = form.get("grant_type")
                with fake.lock:
                    fake.token_requests.append({k: form.get(k) for k in ("grant_type", "refresh_token", "code") if k in form})
                    mode = fake.mode
                    if grant == "refresh_token" and mode in (503, 429):
                        return self.send(mode, b"try again later")
                    if grant == "refresh_token" and mode == 400:
                        return self.send(400, {"error": "invalid_grant", "error_description": "Refresh token revoked"})
                    presented = form.get("refresh_token") or form.get("code")
                    # Rotating refresh tokens: a spent one revokes the grant.
                    if grant == "refresh_token" and (presented in fake.spent or presented != fake.current):
                        return self.send(400, {"error": "invalid_grant", "error_description": "Refresh token reused"})
                    if grant == "authorization_code" and presented != "abc":
                        return self.send(400, {"error": "invalid_grant", "error_description": "Bad code"})
                    fake.spent.add(presented)
                    fake.counter += 1
                    access, refresh = f"tok-{fake.counter}", f"ref-{fake.counter}"
                    fake.valid.add(access)
                    fake.current = refresh
                return self.send(200, {"access_token": access, "token_type": "Bearer", "expires_in": 3600,
                                       "refresh_token": refresh})

        self.lock = threading.Lock()
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_address[1]
        self.base = f"http://127.0.0.1:{self.port}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.reset()

    def reset(self):
        with self.lock:
            self.valid, self.spent, self.current, self.counter = set(), set(), "ref-1", 1
            self.mode = None
            self.token_requests, self.registrations, self.bearers = [], [], []

    @property
    def url(self):
        return f"{self.base}/mcp"


_FAKE = None


def fake():
    global _FAKE
    if _FAKE is None:
        _FAKE = FakeRemote()
    _FAKE.reset()
    return _FAKE


def integration_id(url):
    return "mcp_" + hashlib.sha1(f"{NAME}\0{url}".encode()).hexdigest()[:16]


def _start(ctx, remote, locations=()):
    ctx.set_script([])
    ctx.make_project(config_extra={"mcp": {"servers": {NAME: {"type": "remote", "url": remote.url}}}})
    for sub in locations:
        import os
        os.makedirs(os.path.join(ctx.project, sub), exist_ok=True)
    ctx.start()
    ctx.create_session()


def _statuses(ctx, directory=None):
    directory = directory or ctx.project
    _, _, data = ctx.server.request("GET", f"/api/mcp?directory={urllib.parse.quote(directory)}",
                                    headers={"x-opencode-directory": directory})
    return {s["name"]: s["status"] for s in (data or {}).get("data", [])} if isinstance(data, dict) else {}


def _wait_status(ctx, want, directory=None, timeout=20):
    deadline = time.time() + timeout
    status = None
    while time.time() < deadline:
        status = _statuses(ctx, directory).get(NAME)
        if status and status.get("status") == want:
            return status
        time.sleep(0.2)
    return status


def _seed(ctx, remote, access="old", refresh="ref-1"):
    """Stores an OAuth login whose access token the server no longer accepts."""
    iid = integration_id(remote.url)
    value = {"type": "oauth", "methodID": iid, "access": access, "refresh": refresh, "expires": 1,
             "metadata": {"serverUrl": remote.url, "tokenType": "Bearer", "issuer": remote.base,
                          "client": {"client_id": "dyn-client", "issuer": remote.base}}}
    ctx.call("credential.create", "POST", "/api/credential", {"integrationID": iid, "label": NAME, "value": value})


def _credentials(ctx, label, remote):
    iid = integration_id(remote.url)
    _, data = ctx.call(label, "GET", "/api/credential", record=False)
    rows = [c for c in (data or {}).get("data", []) if c.get("integrationID") == iid]
    ctx.rec(label, rows)


@scenario("mcp227", timeout=120)
def mcp227_oauth_refresh_rotation(ctx):
    """Two Locations connect with the same expired login: the refresh token is spent once and both connect."""
    remote = fake()
    _start(ctx, remote, locations=("other",))
    other = ctx.project + "/other"
    ctx.rec("before", {"here": _wait_status(ctx, "needs_auth"), "other": _wait_status(ctx, "needs_auth", other)})
    _seed(ctx, remote)
    ctx.rec("after", {"here": _wait_status(ctx, "connected"), "other": _wait_status(ctx, "connected", other)})
    time.sleep(0.5)
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))
    _credentials(ctx, "credentials", remote)
    ctx.call("mcp.list", "GET", f"/api/mcp?directory={ctx.project}")


@scenario("mcp227", timeout=120)
def mcp227_oauth_refresh_unavailable(ctx):
    """A 503 or 429 from the token endpoint fails the connection and keeps the login; the next connect refreshes
    with it. A rejected refresh token still signs out."""
    remote = fake()
    remote.mode = 503
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    _seed(ctx, remote)
    ctx.rec("unavailable.503", _wait_status(ctx, "failed"))
    _credentials(ctx, "credentials.503", remote)
    remote.mode = 429
    ctx.call("connect.429", "POST", f"/api/experimental/mcp/{NAME}/connect?directory={ctx.project}")
    ctx.rec("unavailable.429", _statuses(ctx).get(NAME))
    remote.mode = None
    ctx.call("connect.ok", "POST", f"/api/experimental/mcp/{NAME}/connect?directory={ctx.project}")
    ctx.rec("recovered", _statuses(ctx).get(NAME))
    # The server stops accepting the new access token and the authorization server revokes the grant.
    with remote.lock:
        remote.valid.clear()
        remote.mode = 400
    ctx.call("connect.revoked", "POST", f"/api/experimental/mcp/{NAME}/connect?directory={ctx.project}")
    # Dropping the login publishes `credential.switched`, which reconnects the server once more.
    ctx.rec("revoked", _wait_status(ctx, "needs_auth"))
    _credentials(ctx, "credentials.revoked", remote)
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))


def _attempt(ctx, label, iid):
    status, data = ctx.call(label, "POST", f"/api/integration/{iid}/connect/oauth", {"methodID": iid}, record=False)
    attempt = (data or {}).get("data", data) if isinstance(data, dict) else {}
    url = urllib.parse.urlparse(attempt.get("url", ""))
    query = urllib.parse.parse_qs(url.query)
    shown = dict(attempt)
    redirect = urllib.parse.urlparse(query.get("redirect_uri", [""])[0])
    # The callback port, PKCE challenge and state are random per attempt.
    shown["url"] = {"endpoint": f"{url.scheme}://{url.netloc}{url.path}",
                    "params": sorted(query), "redirect": f"{redirect.scheme}://{redirect.hostname}<port>{redirect.path}"}
    ctx.rec(label, {"status": status, "body": shown})
    return attempt, query.get("redirect_uri", [""])[0], query.get("state", [""])[0]


def _complete(ctx, label, iid, attempt, code):
    ctx.call(label, "POST", f"/api/integration/{iid}/connect/oauth/{attempt['attemptID']}/complete", {"code": code})


def _attempt_status(ctx, label, iid, attempt, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, data = ctx.call(label, "GET", f"/api/integration/{iid}/connect/oauth/{attempt['attemptID']}", record=False)
        if isinstance(data, dict) and data.get("data", data).get("status") != "pending":
            break
        time.sleep(0.2)
    ctx.call(label, "GET", f"/api/integration/{iid}/connect/oauth/{attempt['attemptID']}")


@scenario("mcp227", timeout=120)
def mcp227_oauth_pasted_redirect(ctx):
    """The MCP sign-in attempt says `paste: true`; a pasted redirect URL is checked like the browser's redirect, a
    wrong paste keeps the attempt waiting with the reason, and a right one finishes the login."""
    remote = fake()
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    iid = integration_id(remote.url)
    attempt, redirect, state = _attempt(ctx, "connect.oauth", iid)
    with remote.lock:
        ctx.rec("registration", sorted(remote.registrations[0]) if remote.registrations else None)
        ctx.rec("registration.logo_uri", remote.registrations[0].get("logo_uri") if remote.registrations else None)
    _complete(ctx, "paste.not_a_url", iid, attempt, "abc")
    _complete(ctx, "paste.other_state", iid, attempt, f"{redirect}?code=abc&state=nope")
    _complete(ctx, "paste.no_code", iid, attempt, f"{redirect}?state={state}")
    ctx.call("attempt.still_pending", "GET", f"/api/integration/{iid}/connect/oauth/{attempt['attemptID']}")
    _complete(ctx, "paste.ok", iid, attempt, f"  {redirect}?code=abc&state={state}\n")
    _attempt_status(ctx, "attempt.done", iid, attempt)
    ctx.rec("mcp", _wait_status(ctx, "connected"))
    # An authorization error in the pasted URL ends the attempt with its description.
    attempt, redirect, state = _attempt(ctx, "connect.oauth.again", iid)
    _complete(ctx, "paste.error", iid, attempt,
              f"{redirect}?error=access_denied&error_description=User%20said%20no&state={state}")
    _attempt_status(ctx, "attempt.failed", iid, attempt)
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))


@scenario("mcp227", timeout=120)
def mcp227_stdio_probe_cwd_and_url(ctx):
    """Legacy stdio servers under `protocol: "auto"` or a pinned revision (#61496): an unanswered probe means a legacy
    server, a closed one fails with the SDK's text. A local server's `cwd` expands `~` to the home (#61817). An
    invalid remote URL fails with `Invalid MCP server URL`."""
    import os
    import sys
    server = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_mcp.py")
    quick = {"startup": 1500}

    def local(*flags, protocol=None):
        config = {"type": "local", "command": [sys.executable, server, *flags], "timeout": quick}
        if protocol:
            config["protocol"] = protocol
        return config

    home = os.path.join(ctx.state, "home")
    marker = [sys.executable, "-c", "import os; open('cwd-marker', 'w').write(os.getcwd())"]
    servers = {
        "auto-ignore": local("--discover-ignore", protocol="auto"),
        "auto-exit": local("--discover-exit", protocol="auto"),
        "pin-ignore": local("--discover-ignore", protocol="2026-07-28"),
        "pin-exit": local("--discover-exit", protocol="2026-07-28"),
        "home": {"type": "local", "command": marker, "cwd": "~/mcp-home", "timeout": quick},
        "bad-url": {"type": "remote", "url": "not a url", "oauth": False},
    }
    ctx.set_script([])
    ctx.make_project(config_extra={"mcp": {"servers": servers}})
    os.makedirs(os.path.join(home, "mcp-home"), exist_ok=True)
    ctx.start()
    ctx.create_session()
    deadline = time.time() + 30
    while time.time() < deadline:
        statuses = _statuses(ctx)
        if statuses and all(s.get("status") != "pending" for s in statuses.values()):
            break
        time.sleep(0.3)
    ctx.call("mcp.list", "GET", f"/api/mcp?directory={ctx.project}")
    ctx.rec("home.cwd", os.path.exists(os.path.join(home, "mcp-home", "cwd-marker")))


@scenario("mcp227", timeout=120)
def mcp227_slow_server_misses_the_first_step(ctx):
    """A model call waits at most 2 s for MCP servers still connecting (#60490): a server that needs 6 s misses the
    first step's tools, a quick one is listed."""
    import os
    import sys
    from parity.loop import run_prompt
    server = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_mcp.py")
    servers = {"quick": {"type": "local", "command": [sys.executable, server]},
               "slow": {"type": "local", "command": [sys.executable, server, "--start-delay=6"]}}
    run_prompt(ctx, [{"text": "Hello"}], config_extra={"mcp": {"servers": servers}, "permission": {"execute": "deny"}},
               after=lambda ctx, sid: ctx.rec("mcp.after", _statuses(ctx)), events=lambda e: not e.get("type", "").startswith("mcp."))
