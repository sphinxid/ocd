"""2.2.7 API, storage and config changes: file reads and media as sandboxed downloads, cross-site API requests
refused, `session.generate` tokens and cost, synthetic/compact input without booting the location, a fresh install
importing the legacy `auth.json`, new config keys (flags, `websearch.use`, MCP `pool`, warming `"auto"`, v1 fixes)."""
import json
import os
import shutil
import urllib.error
import urllib.parse
import urllib.request

from parity.registry import scenario

DOWNLOAD_HEADERS = ["content-type", "content-disposition", "content-security-policy", "x-content-type-options",
                    "accept-ranges", "content-range", "content-length", "cache-control"]


def boot(ctx, files=None, config_extra=None):
    ctx.set_script([])
    ctx.make_project(files=files, config_extra=config_extra)
    ctx.start()


def raw(ctx, method, path, headers=None, auth=True):
    """A request whose response headers are recorded (`DOWNLOAD_HEADERS` plus CORS)."""
    req = urllib.request.Request(ctx.server.url + path, method=method)
    if auth:
        req.add_header("authorization", ctx.server.auth)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            status, hdrs, body = resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        status, hdrs, body = e.code, e.headers, e.read()
    keep = {k: hdrs.get(k) for k in DOWNLOAD_HEADERS + ["access-control-allow-origin"] if hdrs.get(k) is not None}
    text = body.decode("utf-8", "replace")
    try:
        text = json.loads(text)
    except ValueError:
        pass
    return {"status": status, "headers": keep, "body": text}


@scenario("api227", hold_for_title=0)
def api227_download_headers(ctx):
    """fs.read answers are downloads: plain text for HTML, scripts and XML, an RFC 6266 filename, the sandboxing CSP,
    also for HEAD, ranges and 304s."""
    files = {
        "page.html": "<!doctype html><script>document.title = 'x'</script>",
        "logo.svg": '<svg xmlns="http://www.w3.org/2000/svg"/>',
        "feed.xml": "<feed/>",
        "app.js": "export {}",
        "photo.png": "png",
        "clip.mp4": "0123456789",
        "naïve 100% (1).txt": "text",
        "dir/nested.md": "# nested",
    }
    boot(ctx, files=files)
    loc = "location%5Bdirectory%5D=" + urllib.parse.quote(ctx.project, safe="")
    for name in files:
        path = f"/api/fs/read/{urllib.parse.quote(name, safe='/')}?{loc}"
        ctx.rec(f"get {name}", raw(ctx, "GET", path))
        ctx.rec(f"head {name}", raw(ctx, "HEAD", path))
    clip = f"/api/fs/read/clip.mp4?{loc}"
    ctx.rec("range", raw(ctx, "GET", clip, {"range": "bytes=2-5"}))
    ctx.rec("not modified", raw(ctx, "GET", clip, {"if-none-match": "*"}))
    ctx.rec("unsatisfiable", raw(ctx, "GET", clip, {"range": "bytes=50-"}))


@scenario("api227", hold_for_title=0)
def api227_cross_site(ctx):
    """API navigations from other sites and API calls from disallowed origins get 403 before auth; native clients,
    same-origin pages, allowed origins, media loads, pairing links and artifact pages pass."""
    boot(ctx, files={"page.html": "<p>hi</p>"})
    loc = "location%5Bdirectory%5D=" + urllib.parse.quote(ctx.project, safe="")
    read = f"/api/fs/read/page.html?{loc}"

    def nav(site):
        return {"sec-fetch-site": site, "sec-fetch-mode": "navigate", "sec-fetch-dest": "document"}

    def cors(origin):
        return {"sec-fetch-site": "cross-site", "sec-fetch-mode": "cors", "origin": origin}

    for label, method, path, headers, auth in [
        ("native", "GET", read, {}, True),
        ("same-origin", "GET", read, nav("same-origin"), True),
        ("typed", "GET", read, nav("none"), True),
        ("cross-site", "GET", read, nav("cross-site"), True),
        ("same-site", "GET", read, nav("same-site"), True),
        ("cross-site.unauthenticated", "GET", "/api/session", nav("cross-site"), False),
        ("cross-site.post", "POST", "/api/path", nav("cross-site"), True),
        ("cors.opencode", "GET", read, cors("https://app.opencode.ai"), True),
        ("cors.localhost", "GET", read, cors("http://localhost:3000"), True),
        ("cors.other", "GET", read, cors("https://example.com"), True),
        ("cors.null", "POST", "/api/path", cors("null"), True),
        ("no-cors.origin", "POST", "/api/path", {"sec-fetch-site": "cross-site", "sec-fetch-mode": "no-cors",
                                                  "origin": "https://example.com"}, True),
        ("media", "GET", read, {"sec-fetch-site": "cross-site", "sec-fetch-mode": "no-cors", "sec-fetch-dest": "video"}, True),
        ("pairing", "GET", "/auth/connect/expired", {**nav("cross-site"), "accept": "text/html"}, False),
        ("artifact.view", "GET", f"/api/experimental/artifacts/art_missing/view/?{loc}", nav("cross-site"), True),
    ]:
        ctx.rec(label, raw(ctx, method, path, headers, auth))


@scenario("api227", hold_for_title=0)
def api227_session_generate(ctx):
    """session.generate answers its tokens and cost and adds them to the session's totals with a
    session.usage.recorded event (source generate); the history is untouched."""
    ctx.set_script([
        {"text": "Hello.", "usage": {"prompt_tokens": 120, "completion_tokens": 7}},
        {"text": "Transient answer", "usage": {"prompt_tokens": 100, "completion_tokens": 10,
                                               "prompt_tokens_details": {"cached_tokens": 30}}},
    ])
    ctx.make_project(config_extra={"providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096},
                          "cost": {"input": 1, "output": 2, "cache": {"read": 0.1, "write": 0.5}}}}}}})
    ctx.start()
    sid = ctx.create_session(title="Generate")
    ctx.prompt(sid, "hi")
    ctx.call("session.generate", "POST", f"/api/session/{sid}/generate", {"prompt": "Summarize privately"})
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")


@scenario("api227", hold_for_title=0)
def api227_stranded_session_input(ctx):
    """synthetic and compact input is admitted for a session whose directory is gone, without booting it."""
    boot(ctx)
    gone = os.path.join(os.path.dirname(ctx.project), "gone-parity")
    shutil.rmtree(gone, ignore_errors=True)
    os.makedirs(gone)
    sid = ctx.data("session.create", "POST", "/api/session", {"location": {"directory": gone}, "title": "Gone"})["id"]
    # A new process, so neither binary has the location loaded when its directory is gone.
    ctx.stop()
    shutil.rmtree(gone)
    ctx.start()
    ctx.call("synthetic", "POST", f"/api/session/{sid}/synthetic",
             {"text": "note", "resume": False, "id": "msg_ffffffffffffSynthetic0001"})
    ctx.call("compact", "POST", f"/api/session/{sid}/compact", {"id": "msg_ffffffffffffCompaction001", "resume": False})
    ctx.call("synthetic.missing", "POST", "/api/session/ses_ffffffffffffZZZZZZZZZZZZZZ/synthetic", {"text": "x"})
    import time
    time.sleep(2)
    ctx.call("inbox", "GET", f"/api/session/{sid}/inbox")
    ctx.record_events()


@scenario("api227", hold_for_title=0)
def api227_legacy_credentials_fresh(ctx):
    """a fresh install imports the legacy auth.json (a non-object file is skipped and setup goes on)."""
    ctx.set_script([])
    ctx.make_project()
    data = os.path.join(ctx.state, "data", "opencode")
    os.makedirs(data, exist_ok=True)
    with open(os.path.join(data, "auth.json"), "w") as f:
        json.dump({
            "openai": {"type": "oauth", "refresh": "refresh", "access": "access", "expires": 123},
            "anthropic": {"type": "api", "key": "sk-legacy"},
            "https://example.com/": {"type": "wellknown", "key": "TOKEN", "token": "wellknown-key"},
            "invalid": {"type": "unknown"},
        }, f)
    ctx.start()
    ctx.call("credentials", "GET", "/api/credential")
    ctx.stop()
    ctx.make_project()
    os.makedirs(data, exist_ok=True)
    with open(os.path.join(data, "auth.json"), "w") as f:
        f.write("[1, 2]")
    ctx.start()
    ctx.call("credentials.malformed", "GET", "/api/credential")


@scenario("api227", hold_for_title=0)
def api227_config_keys(ctx):
    """config accepts and echoes 2.2.7's keys in its order: the flag registry and all_flags, part_rows, the codemodeVm
    alias, MCP pool, warming "auto", provider cache.warming, websearch.use (which removes web search providers), and
    v1 provider fixes (inline variants, partial limits and capabilities, options.baseURL over api)."""
    boot(ctx, config_extra={
        "experimental": {"subagent_depth": 2, "part_rows": True, "codemodeVm": True, "console_watches": False,
                         "artifacts": False, "all_flags": False, "portable_shell_scanner": True},
        "warming": {"interval": "auto", "duration": "30 minutes"},
        "mcp": {"servers": {"pooled": {"type": "remote", "url": "http://127.0.0.1:9/mcp", "pool": True,
                                       "disabled": True}}},
        "policies": [{"action": "websearch.use", "resource": "*", "effect": "deny"},
                     {"action": "websearch.use", "resource": "exa", "effect": "allow"}],
        "provider": {"legacy": {"api": "https://api.example.com", "options": {"baseURL": "https://own.example.com"},
                                "models": {"m": {"limit": {"output": 10}, "tool_call": False}}}},
        "agent": {"reviewer": {"model": "mock/m1#high", "variant": "low"}},
    })
    ctx.booted()
    ctx.call("config", "GET", "/api/config")
    ctx.call("websearch.providers", "GET", "/api/websearch/provider")
    ctx.call("agent.reviewer", "GET", "/api/agent/reviewer")
    ctx.settle("/api/provider")
    ctx.call("provider.legacy", "GET", "/api/provider/legacy")
