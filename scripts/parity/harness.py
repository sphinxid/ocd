"""Shared parity machinery: isolated server processes, SSE capture, the normalizer and the per-scenario context.

Both binaries run every scenario with *the same paths* (project directory and XDG state root are recreated at the
same location before each run), so path-derived values (project IDs, data directories) are directly comparable and
only genuinely volatile values need masking.
"""
import base64
import json
import os
import re
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request

PASSWORD = "parity"

# ---------------------------------------------------------------------------------------------------------------
# Normalizer. Only values that differ between two runs of the *same* binary are masked:
#   - generated IDs (12 hex time+counter + 14 random base62), renumbered by first appearance;
#   - UUIDs;
#   - epoch-millisecond timestamps (numbers > 1e12) and ISO-8601 timestamps under any key;
#   - durations under duration/elapsed keys;
#   - session slugs (random word pairs);
#   - the application version string (dev build vs port);
#   - absolute paths of the per-run project/state directories (identical between runs, masked for readability);
#   - opaque base64url cursors (decoded and normalized instead of masked);
#   - the random trace/span IDs inside `b3`/`traceparent` header values (Effect's traced HTTP client), renumbered by
#     first appearance so which requests share a trace or span stays visible; the headers themselves are compared.
# Anything else is compared verbatim.
# ---------------------------------------------------------------------------------------------------------------
# Derived IDs append a suffix (`msg_<id>_4` for forked messages), so only a following alphanumeric ends a match.
ID_RE = re.compile(r"\b([a-z]{2,6})_([0-9a-f]{12}[0-9A-Za-z]{14})(?![0-9A-Za-z])")
UUID_RE = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
ISO_RE = re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?\b")
VERSION_RE = re.compile(r"^(0\.0\.0-v2-\d+|0\.0\.0-[\w.-]+|2\.0\.\d+)$")
DURATION_KEYS = {"duration", "elapsed", "durationMs", "ms", "uptime"}
VOLATILE_KEYS = {"pid"}  # OS process ids
# User-Agent `opencode/<channel>/<version>/<app>`: channel and version come from the build (dev build "v2" +
# 0.0.0-v2-<timestamp> upstream, "latest" + workspace version in the port).
BUILD_RE = re.compile(r"\b(0\.0\.0-v2-\d{12}|2\.0\.26)\b")
UA_RE = re.compile(r"opencode/(v2|latest|local|dev)/(0\.0\.0-[\w.-]+|2\.0\.\d+)/")
# `b3: <trace>-<span>-<sampled>[-<parent>]` and `traceparent: 00-<trace>-<span>-<flags>` (HttpTraceContext.toHeaders).
B3_RE = re.compile(r"^([0-9a-f]{32})-([0-9a-f]{16})-([01d])(?:-([0-9a-f]{16}))?$")
TRACEPARENT_RE = re.compile(r"^00-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$")


class Normalizer:
    def __init__(self, paths):
        # Longest first so nested paths are replaced before their parents.
        self.paths = sorted(paths.items(), key=lambda kv: -len(kv[1]))
        self.ids = {}
        self.uuids = {}
        self.traces = {}
        self.spans = {}

    def _trace(self, kind, value):
        table = self.traces if kind == "trace" else self.spans
        if value not in table:
            table[value] = f"<{kind}{len(table) + 1}>"
        return table[value]

    def trace_header(self, key, value):
        if key == "b3" and (m := B3_RE.match(value)):
            out = f"{self._trace('trace', m.group(1))}-{self._trace('span', m.group(2))}-{m.group(3)}"
            return out + (f"-{self._trace('span', m.group(4))}" if m.group(4) else "")
        if key == "traceparent" and (m := TRACEPARENT_RE.match(value)):
            return f"00-{self._trace('trace', m.group(1))}-{self._trace('span', m.group(2))}-{m.group(3)}"
        return None

    def _id(self, m):
        key = m.group(0)
        if key not in self.ids:
            n = sum(1 for k in self.ids if k.startswith(m.group(1) + "_")) + 1
            self.ids[key] = f"<{m.group(1)}{n}>"
        return self.ids[key]

    def _uuid(self, m):
        key = m.group(0)
        if key not in self.uuids:
            self.uuids[key] = f"<uuid{len(self.uuids) + 1}>"
        return self.uuids[key]

    def text(self, s):
        for name, p in self.paths:
            if p:
                s = s.replace(p, f"<{name}>")
        s = ID_RE.sub(self._id, s)
        s = UUID_RE.sub(self._uuid, s)
        s = ISO_RE.sub("<iso>", s)
        s = UA_RE.sub("opencode/<channel>/<version>/", s)
        # Build version/channel embedded in text (e.g. the built-in report skill's diagnostics).
        s = BUILD_RE.sub("<version>", s)
        s = re.sub(r"install/channel: (v2|latest)", "install/channel: <channel>", s)
        # Plugin failure references (`err_` + 8 random hex digits).
        s = re.sub(r"\berr_[0-9a-f]{8}\b", "<err-ref>", s)
        # The module that imported a missing AI SDK package: upstream's bundle chunk inside the compiled binary
        # (`/$bunfs/root/chunk-<hash>.js`) vs ocd's sidecar bundle (`.../ocd-plugin-host/<hash>/host.js`).
        s = re.sub(r"imported from (/\$bunfs/root/chunk-[0-9a-z]+\.js|\S*/ocd-plugin-host/[0-9a-f]+/host\.js)",
                   "imported from <bundle>", s)
        return s

    def __call__(self, value, key=None):
        if isinstance(value, dict):
            # Keys can be IDs too (e.g. `session.active` maps session IDs to status).
            return {(self.text(k) if ID_RE.search(k) else k): self(v, k) for k, v in value.items()}
        if isinstance(value, list):
            return [self(v, key) for v in value]
        if isinstance(value, bool) or value is None:
            return value
        if isinstance(value, (int, float)):
            if key in VOLATILE_KEYS:
                return f"<{key}>"
            if value > 1e12 and value < 1e14:
                return "<t>"
            if key in DURATION_KEYS:
                return "<dur>"
            return value
        if isinstance(value, str):
            if key == "slug":
                return "<slug>"
            if key in ("b3", "traceparent") and (masked := self.trace_header(key, value)) is not None:
                return masked
            if VERSION_RE.match(value):
                return "<version>"
            if re.fullmatch(r"[A-Za-z0-9_-]{24,}", value) and not ID_RE.fullmatch(value):
                try:
                    decoded = base64.urlsafe_b64decode(value + "=" * (-len(value) % 4)).decode()
                    return "<cursor " + json.dumps(self(json.loads(decoded)), sort_keys=True) + ">"
                except Exception:
                    pass
            return self.text(value)
        return value


# ---------------------------------------------------------------------------------------------------------------
# Server process
# ---------------------------------------------------------------------------------------------------------------
def isolated_env(state_root, extra=None):
    env = {k: v for k, v in os.environ.items() if not k.startswith("OPENCODE")}
    for key, sub in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                     ("XDG_CACHE_HOME", "cache")]:
        env[key] = os.path.join(state_root, sub)
        os.makedirs(env[key], exist_ok=True)
    home = os.path.join(state_root, "home")
    os.makedirs(home, exist_ok=True)
    # SAFETY: disable the background service for every channel, so CLI commands always use a private
    # standalone server and never reach (or start) a service on the default port, which may be the user's.
    service_dir = os.path.join(env["XDG_CONFIG_HOME"], "opencode")
    os.makedirs(service_dir, exist_ok=True)
    for name in ("service.json", "service-v2.json", "service-local.json"):
        with open(os.path.join(service_dir, name), "w") as f:
            f.write('{"disabled": true}\n')
    env.update({
        "HOME": home,
        "OPENCODE_TEST_HOME": home,
        "OPENCODE_DISABLE_MODELS_FETCH": "1",
        "OPENCODE_DISABLE_FILEWATCHER": "1",
        "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "GIT_CONFIG_NOSYSTEM": "1",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "TZ": "UTC",
        "LANG": "C.UTF-8",
    })
    env.update(extra or {})
    return env


class Server:
    def __init__(self, binary, cwd, env, name):
        self.name = name
        env = dict(env)
        env["OPENCODE_PASSWORD"] = PASSWORD
        self.proc = subprocess.Popen([binary, "serve", "--port", "0"], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.url = None
        self.output = []
        deadline = time.time() + 60
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                break
            self.output.append(line)
            m = re.search(r"https?://[\w.:\[\]]+", line)
            if m and "listening" in line:
                self.url = m.group(0)
                break
        if not self.url:
            self.close()
            raise RuntimeError(f"{name}: server did not report a URL:\n{''.join(self.output)}")
        threading.Thread(target=self._drain, daemon=True).start()
        self.auth = "Basic " + base64.b64encode(f"opencode:{PASSWORD}".encode()).decode()
        self.events = []
        self.cond = threading.Condition()
        self.sse_ready = threading.Event()
        threading.Thread(target=self._sse, daemon=True).start()
        self.sse_ready.wait(10)
        time.sleep(0.3)

    def _drain(self):
        for line in self.proc.stdout:
            self.output.append(line)

    def request(self, method, path, body=None, headers=None, auth=True, raw_body=None, timeout=60):
        data = raw_body if raw_body is not None else (None if body is None else json.dumps(body).encode())
        req = urllib.request.Request(self.url + path, data=data, method=method)
        if auth:
            req.add_header("authorization", self.auth)
        if data is not None:
            req.add_header("content-type", "application/json")
        for k, v in (headers or {}).items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.status, resp.headers.get("content-type"), _decode(resp.read())
        except urllib.error.HTTPError as e:
            return e.code, e.headers.get("content-type"), _decode(e.read())

    def _sse(self):
        req = urllib.request.Request(self.url + "/api/event")
        req.add_header("authorization", self.auth)
        try:
            with urllib.request.urlopen(req) as resp:
                self.sse_ready.set()
                for raw in resp:
                    line = raw.decode("utf-8", "replace").rstrip("\n")
                    if not line.startswith("data:"):
                        continue
                    try:
                        ev = json.loads(line[5:].strip())
                    except Exception:
                        ev = {"unparsed": line}
                    with self.cond:
                        self.events.append(ev)
                        self.cond.notify_all()
        except Exception:
            self.sse_ready.set()

    def wait_event(self, pred, timeout=30, start=0):
        deadline = time.time() + timeout
        with self.cond:
            while True:
                for i in range(start, len(self.events)):
                    if pred(self.events[i]):
                        return self.events[i]
                left = deadline - time.time()
                if left <= 0:
                    return None
                self.cond.wait(left)

    def close(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except Exception:
                self.proc.kill()
                self.proc.wait(5)


def _decode(raw):
    if not raw:
        return None
    try:
        return json.loads(raw)
    except Exception:
        return raw.decode("utf-8", "replace")


# ---------------------------------------------------------------------------------------------------------------
# Scenario context
# ---------------------------------------------------------------------------------------------------------------
TERMINAL = ("session.execution.succeeded", "session.execution.failed", "session.execution.interrupted")


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="Parity", GIT_AUTHOR_EMAIL="parity@example.com",
               GIT_COMMITTER_NAME="Parity", GIT_COMMITTER_EMAIL="parity@example.com",
               GIT_AUTHOR_DATE="2026-01-01T00:00:00Z", GIT_COMMITTER_DATE="2026-01-01T00:00:00Z",
               GIT_CONFIG_NOSYSTEM="1", HOME=cwd)
    subprocess.run(["git", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", *args], cwd=cwd, env=env,
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


class Ctx:
    """Everything a scenario needs for one binary run. Scenarios call `rec`/`call`; the dump is built from `log`."""

    def __init__(self, name, binary, work, mock, opts):
        self.name = name  # "upstream" | "rust"
        self.binary = binary
        self.work = work
        self.project = os.path.join(work, "project")
        self.state = os.path.join(work, "state")
        self.mock = mock
        self.opts = opts
        self.log = []
        self.server = None
        self.server_url = None
        self.event_start = 0
        self.env_extra = {}

    def set_script(self, script):
        """Install a mock script; "$PROJECT" in any string is replaced by the project path."""
        def sub(v):
            if isinstance(v, str):
                return v.replace("$PROJECT", self.project)
            if isinstance(v, list):
                return [sub(x) for x in v]
            if isinstance(v, dict):
                return {k: sub(x) for k, x in v.items()}
            return v
        self.mock.set_script(sub(script))

    # ---- setup -------------------------------------------------------------------------------------------------
    def make_project(self, files=None, config=True, git_repo=False, config_extra=None):
        for d in (self.project, self.state):
            shutil.rmtree(d, ignore_errors=True)
            os.makedirs(d)
        if config:
            cfg = {"model": "mock/m1", "providers": {"mock": {
                "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": f"http://127.0.0.1:{self.mock.port}/v1", "apiKey": "test"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
            if isinstance(config, dict):
                cfg = config
            for k, v in (config_extra or {}).items():
                cfg[k] = v
            files = dict(files or {})
            files.setdefault("opencode.json", json.dumps(cfg, indent=2))
        for rel, content in (files or {}).items():
            path = os.path.join(self.project, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(content)
        if git_repo:
            git(self.project, "init", "-q")
            git(self.project, "add", "-A")
            git(self.project, "commit", "-q", "-m", "initial")

    def env(self):
        return isolated_env(self.state, self.env_extra)

    def start(self):
        self.server = Server(self.binary, self.project, self.env(), self.name)
        self.server_url = self.server.url
        self.event_start = len(self.server.events)
        return self.server

    def stop(self):
        if self.server:
            self.server.close()
            self.server = None

    # ---- recording ---------------------------------------------------------------------------------------------
    def rec(self, label, value):
        self.log.append((label, value))

    def call(self, label, method, path, body=None, record=True, **kw):
        kw.setdefault("headers", {"x-opencode-directory": self.project})
        status, ctype, data = self.server.request(method, path, body, **kw)
        if record:
            self.rec(label, {"status": status, "content-type": (ctype or "").split(";")[0], "body": data})
        return status, data

    def data(self, label, method, path, body=None, **kw):
        _, data = self.call(label, method, path, body, **kw)
        return data.get("data", data) if isinstance(data, dict) else data

    def create_session(self, label="session.create", **body):
        body.setdefault("location", {"directory": self.project})
        sid = self.data(label, "POST", "/api/session", body)["id"]
        self.booted()
        return sid

    def settle(self, path, timeout=6.0, gap=0.4):
        """Poll a GET endpoint until two consecutive answers agree (unrecorded). Upstream builds several
        location services lazily and fills them from asynchronously activated plugins: the first agent.list
        of a location answers [], the next ones built-ins only, and config-provided agents/commands/skills
        appear after the config plugins' debounced reload. The port answers the converged value at once."""
        deadline = time.time() + timeout
        last = object()
        while time.time() < deadline:
            _, _, data = self.server.request("GET", path, headers={"x-opencode-directory": self.project})
            if data == last:
                return
            last = data
            time.sleep(gap)

    def booted(self, timeout=10):
        """Wait until the location has finished booting. Upstream activates its built-in plugins
        asynchronously after the first request for a location and announces `plugin.updated` when done;
        requests racing that boot see no agents (e.g. permission asks evaluate to deny). The port boots
        synchronously and emits the same events, so waiting is a no-op there."""
        if not getattr(self, "_booted", False):
            # A location-scoped read boots the location in both binaries; settling the agent list also waits
            # out upstream's lazy Agent state (its first agent.list answers [] and permission asks racing it
            # evaluate to deny; plugin event subscriptions such as plan reminders start late).
            self.settle(f"/api/agent?directory={self.project}")
            self._booted = self.wait_for("plugin.updated", timeout=timeout) is not None

    def prompt(self, sid, text, label="session.prompt", wait=True, timeout=60, **extra):
        mark = len(self.server.events)
        self.call(label, "POST", f"/api/session/{sid}/prompt", {"text": text, **extra})
        if wait:
            self.wait_idle(sid, start=mark, timeout=timeout)

    def wait_idle(self, sid, start=None, timeout=60, settle=1.0):
        ev = self.server.wait_event(lambda e: e.get("type") in TERMINAL and e.get("data", {}).get("sessionID") == sid,
                                    timeout=timeout, start=self.event_start if start is None else start)
        if ev is None:
            self.rec("TIMEOUT waiting for idle", sid)
        time.sleep(settle)  # trailing ephemeral events (title, usage)
        return ev

    def wait_for(self, type_, timeout=30, start=None, pred=None):
        return self.server.wait_event(lambda e: e.get("type") == type_ and (pred is None or pred(e)), timeout=timeout,
                                      start=self.event_start if start is None else start)

    def events(self):
        return list(self.server.events[self.event_start:])

    def record_events(self, label="events", filt=None):
        evs = [e for e in self.events() if filt is None or filt(e)]
        durable, ephemeral = {}, []
        for e in evs:
            agg = (e.get("durable") or {}).get("aggregateID")
            if agg:
                durable.setdefault(agg, []).append(e)
            else:
                ephemeral.append(e)
        out = []
        for agg, items in durable.items():
            out.extend(sorted(items, key=lambda e: e["durable"].get("seq", 0)))
        self.rec(label + ".durable", out)
        self.rec(label + ".ephemeral", ephemeral)

    def record_llm(self, label="llm", full=False):
        """Record what each binary sent to the mock provider. By default only the conversation messages and the tool
        names; `full` adds the system prompt, tool schemas, headers and every request parameter."""
        out = []
        for r in self.mock.main_requests():
            body = r["body"]
            if not isinstance(body, dict):
                out.append(body)
                continue
            if full:
                hdr = {k.lower(): v for k, v in r["headers"].items()
                       if k.lower() not in ("content-length", "host", "connection", "accept-encoding")}
                out.append({"path": r["path"], "headers": hdr, "body": body})
            else:
                msgs = [m for m in body.get("messages", []) if m.get("role") != "system"]
                tools = sorted(t.get("function", {}).get("name", "?") for t in body.get("tools", []) or [])
                out.append({"messages": msgs, "tools": tools})
        self.rec(label, out)

    # ---- CLI ---------------------------------------------------------------------------------------------------
    def cli(self, label, args, cwd=None, stdin=None, timeout=60, env_extra=None):
        env = self.env()
        env.update(env_extra or {})
        try:
            p = subprocess.run([self.binary, *args], cwd=cwd or self.project, env=env, input=stdin,
                               capture_output=True, text=True, timeout=timeout)
            res = {"exit": p.returncode, "stdout": p.stdout, "stderr": p.stderr}
        except subprocess.TimeoutExpired as e:
            res = {"exit": "TIMEOUT", "stdout": _s(e.stdout), "stderr": _s(e.stderr)}
        self.rec(label, res)
        return res


def _s(v):
    if v is None:
        return ""
    return v.decode("utf-8", "replace") if isinstance(v, bytes) else v


def dump(ctx, prog_name=None):
    norm = Normalizer({"root": ctx.project, "state": ctx.state, "work": ctx.work, "server": ctx.server_url or ""})
    lines = []
    for label, value in ctx.log:
        if isinstance(value, dict) and {"stdout", "stderr"} <= value.keys():
            value = {k: (_cli_text(v, prog_name) if isinstance(v, str) else v) for k, v in value.items()}
        n = norm(value)
        if isinstance(n, dict) and {"stdout", "stderr", "exit"} <= n.keys():
            lines.append(f"== {label} exit={n['exit']}")
            lines.extend("   out| " + l for l in n["stdout"].splitlines())
            lines.extend("   err| " + l for l in n["stderr"].splitlines())
        elif isinstance(n, list) and label.endswith((".durable", ".ephemeral")):
            lines.append(f"== {label} ({len(n)})")
            lines.extend("   " + json.dumps(e, sort_keys=True) for e in n)
        else:
            lines.append(f"== {label}")
            lines.extend("   " + l for l in json.dumps(n, sort_keys=True, indent=1).splitlines())
    return lines


def _cli_text(text, prog_name):
    if prog_name:
        text = _prog(text, prog_name)
    # Epoch-millisecond timestamps inside CLI JSON lines (`"timestamp":1791850080345`).
    return re.sub(r"(?<![\d.])1[789]\d{11}(?!\d)", "<t>", text)


def _prog(text, prog_name):
    # By-design rename: the port's binary is `ocd`. The program name is rewritten in command position and in
    # the identifiers shell-completion scripts derive from it (`_ocd()`, `###-begin-ocd-completions-###`).
    name = re.escape(prog_name)
    text = re.sub(rf"(?<![\w/.-]){name}(?=\s|$)", "opencode", text)
    return re.sub(rf"(?<=[_-]){name}(?=[-_(\s]|$)", "opencode", text)
