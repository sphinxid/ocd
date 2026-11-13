"""The local model discovery plugins (core/src/plugin/provider/{local,lmstudio,ollama,vllm}.ts) against mock LM Studio,
Ollama and vLLM servers: discovery through configured `baseURL`s, the provider, model and integration lists it
derives, a chat request through a discovered model, an `apiKey`, a server that goes down (the last inventory is kept
until the next answer) and a configuration change (the plugins forget their inventory and ask the new endpoint).

The mocks listen on ephemeral ports chosen once per suite run, so both binaries see the same URLs. The default
origins (1234, 11434, 8000) are never used: every scenario configures all three providers, and the harness's
global config points them at a dead port for every other scenario (`harness.LOCAL_PROVIDERS`)."""
import json
import os
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from parity.registry import scenario

PROVIDERS = ("lmstudio", "ollama", "vllm")


def _port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


PORTS = {}


def port(name):
    """The suite-wide port of mock `name` (the same for both binaries)."""
    if name not in PORTS:
        PORTS[name] = _port()
    return PORTS[name]


class LocalServer:
    """A mock local model server. `state` holds what it serves; `down` answers 503 to everything."""

    def __init__(self, kind, models, name=None):
        self.kind = kind
        self.models = models
        self.down = False
        self.requests = []
        self.lock = threading.Lock()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _answer(self, status, body=None, ctype="application/json"):
                data = b"" if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
                self.send_response(status)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _handle(self, method):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw) if raw else None
                except Exception:
                    body = raw.decode("utf-8", "replace")
                with outer.lock:
                    outer.requests.append({"method": method, "path": self.path,
                                           "authorization": self.headers.get("authorization"),
                                           "accept": self.headers.get("accept"), "body": body})
                status, out, ctype = outer.route(method, self.path, body)
                self._answer(status, out, ctype)

            def do_GET(self):
                self._handle("GET")

            def do_POST(self):
                self._handle("POST")

        ThreadingHTTPServer.allow_reuse_address = True
        self.httpd = ThreadingHTTPServer(("127.0.0.1", port(name or kind)), Handler)
        self.httpd.daemon_threads = True
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    @property
    def origin(self):
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def route(self, method, path, body):
        if self.down:
            return 503, {"error": "unavailable"}, "application/json"
        path = path.split("?")[0]
        if path.endswith("/chat/completions"):
            return 200, chat_stream(body), "text/event-stream"
        if self.kind == "lmstudio" and path.endswith("/api/v1/models"):
            return 200, {"models": self.models}, "application/json"
        if self.kind == "ollama" and path.endswith("/api/tags"):
            return 200, {"models": [m["summary"] for m in self.models]}, "application/json"
        if self.kind == "ollama" and path.endswith("/api/show"):
            name = (body or {}).get("model")
            for m in self.models:
                if m["summary"]["model"] == name and m.get("show") is not None:
                    return 200, m["show"], "application/json"
            return 404, {"error": "model not found"}, "application/json"
        if self.kind == "vllm" and path.endswith("/health"):
            return 200, None, "text/plain"
        if self.kind == "vllm" and path.endswith("/v1/models"):
            return 200, {"object": "list", "data": self.models}, "application/json"
        return 404, {"error": "not found"}, "application/json"

    def seen(self, chat=False):
        """The distinct discovery (or chat) requests, sorted (how often each repeats is timing)."""
        with self.lock:
            reqs = list(self.requests)
        out = []
        for r in reqs:
            is_chat = r["path"].endswith("/chat/completions")
            if is_chat != chat:
                continue
            item = {"method": r["method"], "path": r["path"], "authorization": r["authorization"], "accept": r["accept"]}
            if r["method"] == "POST" and not is_chat:
                item["body"] = r["body"]
            if is_chat and isinstance(r["body"], dict):
                item["model"] = r["body"].get("model")
                item["stream"] = r["body"].get("stream")
            key = json.dumps(item, sort_keys=True)
            if key not in [json.dumps(o, sort_keys=True) for o in out]:
                out.append(item)
        return sorted(out, key=lambda o: json.dumps(o, sort_keys=True))

    def clear(self):
        with self.lock:
            self.requests.clear()


def chat_stream(body):
    stream = isinstance(body, dict) and body.get("stream")
    if not stream:
        return {"id": "local", "object": "chat.completion", "created": 1, "model": (body or {}).get("model"),
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Local answer."},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}
    chunks = [
        {"id": "local", "object": "chat.completion.chunk", "created": 1, "model": body.get("model"),
         "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Local answer."}}]},
        {"id": "local", "object": "chat.completion.chunk", "created": 1, "model": body.get("model"),
         "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
         "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}},
    ]
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks).encode() + b"data: [DONE]\n\n"


# ---- inventories ----------------------------------------------------------------------------------------------
def lm_model(key, name, arch=None, contexts=(), max_ctx=32768, vision=None, tools=None, type_="llm"):
    m = {"type": type_, "key": key, "display_name": name,
         "loaded_instances": [{"config": {"context_length": c}} for c in contexts], "max_context_length": max_ctx}
    if arch is not None:
        m["architecture"] = arch
    if vision is not None:
        m["capabilities"] = {"vision": vision, "trained_for_tool_use": bool(tools)}
    return m


LMSTUDIO = [
    lm_model("qwen/qwen3-coder-30b", "Qwen3 Coder 30B", "qwen3moe", (32768, 16384), 262144, False, True),
    lm_model("google/gemma-3-12b", "Gemma 3 12B", "gemma3", (), 131072, True, False),
    lm_model("zero-context", "", None, (), 0),
    lm_model("text-embedding-nomic", "Nomic Embed", "nomic-bert", (), 2048, type_="embedding"),
]


def ollama_model(name, digest, family, caps, context, show_family=None):
    details = {"format": "gguf", "family": family, "families": [family], "parameter_size": "8B",
               "quantization_level": "Q4_K_M"}
    summary = {"name": name, "model": name, "modified_at": "2026-01-01T00:00:00Z", "size": 1000000, "digest": digest,
               "details": details}
    show = None
    if caps is not None:
        show = {"parameters": "temperature 0.7", "capabilities": caps,
                "model_info": {"general.architecture": family, f"{show_family or family}.context_length": context}}
        if show_family is not None:
            show["details"] = dict(details, family=show_family)
    return {"summary": summary, "show": show}


OLLAMA = [
    ollama_model("llama3.2:3b", "d1", "llama", ["completion", "tools"], 131072),
    ollama_model("gemma3:4b", "d2", "gemma3", ["completion", "vision"], 0, show_family=""),
    ollama_model("nomic-embed-text:latest", "d3", "nomic-bert", ["embedding"], 8192),
    ollama_model("broken:1b", "d4", "llama", None, 0),
]


def vllm_model(id, max_len, owned_by="vllm"):
    return {"id": id, "object": "model", "created": 1, "owned_by": owned_by, "root": id, "parent": None,
            "max_model_len": max_len, "permission": []}


VLLM = [vllm_model("Qwen/Qwen3-8B", 40960), vllm_model("meta-llama/Llama-3.1-8B", None),
        vllm_model("someone-elses", 4096, owned_by="other")]


# ---- helpers --------------------------------------------------------------------------------------------------
def providers_config(servers, keys=None, paths=None):
    keys = keys or {}
    paths = paths or {}
    out = {}
    for name in PROVIDERS:
        settings = {"baseURL": servers[name].origin + paths.get(name, "/v1")}
        if name in keys:
            settings["apiKey"] = keys[name]
        out[name] = {"settings": settings}
    return out


def start_servers(models=None):
    models = models or {}
    return {name: LocalServer(name, list(models.get(name, default))) for name, default in
            (("lmstudio", LMSTUDIO), ("ollama", OLLAMA), ("vllm", VLLM))}


def boot(ctx, servers, keys=None, paths=None, extra=None, watch=False):
    if watch:
        ctx.env_extra["OPENCODE_DISABLE_FILEWATCHER"] = "0"
    ctx.set_script([])
    cfg = {"model": "mock/m1", "providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
    cfg["providers"].update(providers_config(servers, keys, paths))
    cfg.update(extra or {})
    ctx.make_project(config=cfg)
    ctx.start()


def local_models(ctx):
    """`model.list` entries of the three providers, by provider then id."""
    _, _, data = ctx.server.request("GET", "/api/model", headers={"x-opencode-directory": ctx.project})
    items = data.get("data", data) if isinstance(data, dict) else data
    if not isinstance(items, list):
        return []
    return sorted([m for m in items if m.get("providerID") in PROVIDERS], key=lambda m: (m["providerID"], m["id"]))


def wait_models(ctx, pred, timeout=20):
    deadline = time.time() + timeout
    models = local_models(ctx)
    while time.time() < deadline and not pred(models):
        time.sleep(0.25)
        models = local_models(ctx)
    return models


def ids(models):
    return [f"{m['providerID']}/{m['id']}" for m in models]


def record_lists(ctx, label):
    """The provider, model and integration lists, restricted to the three local providers."""
    _, _, data = ctx.server.request("GET", "/api/provider", headers={"x-opencode-directory": ctx.project})
    items = data.get("data", data) if isinstance(data, dict) else data
    ctx.rec(label + ".providers", sorted([p for p in items if p.get("id") in PROVIDERS], key=lambda p: p["id"]))
    ctx.rec(label + ".models", local_models(ctx))
    _, _, data = ctx.server.request("GET", "/api/integration", headers={"x-opencode-directory": ctx.project})
    items = data.get("data", data) if isinstance(data, dict) else data
    ctx.rec(label + ".integrations", sorted([i for i in items if i.get("id") in PROVIDERS], key=lambda i: i["id"]))


def record_requests(ctx, servers, label):
    ctx.rec(label, {name: servers[name].seen() for name in PROVIDERS})


def updates(ctx, start, label):
    """The `*.updated` events since `start`, as a multiset (their interleaving with the configuration reload's own
    notifications is a race on both sides)."""
    kinds = ("integration.updated", "provider.updated", "model.updated")
    evs = [e.get("type") for e in ctx.server.events[start:] if e.get("type") in kinds]
    ctx.rec(label, {k: evs.count(k) for k in kinds})


def close(servers):
    for s in servers.values():
        s.close()


EXPECTED = ["lmstudio/google/gemma-3-12b", "lmstudio/qwen/qwen3-coder-30b", "lmstudio/zero-context",
            "ollama/gemma3:4b", "ollama/llama3.2:3b", "vllm/Qwen/Qwen3-8B", "vllm/meta-llama/Llama-3.1-8B"]


# ---- scenarios ------------------------------------------------------------------------------------------------
@scenario("local", hold_for_title=0)
def local_discovery(ctx):
    """LM Studio, Ollama and vLLM at configured endpoints: discovery requests, provider/model/integration lists."""
    servers = start_servers()
    try:
        boot(ctx, servers)
        models = wait_models(ctx, lambda m: ids(m) == EXPECTED)
        ctx.rec("discovered", ids(models))
        record_lists(ctx, "lists")
        record_requests(ctx, servers, "requests")
        # The boot's notifications plus one reload per discovered provider (the order interleaves with the boot).
        time.sleep(0.5)
        updates(ctx, ctx.event_start, "boot.updates")
    finally:
        close(servers)


@scenario("local", hold_for_title=0)
def local_api_key(ctx):
    """a configured `apiKey` is sent as a bearer token on discovery and kept in the provider settings."""
    servers = start_servers()
    try:
        boot(ctx, servers, keys={"lmstudio": "lm-secret", "ollama": "", "vllm": "vllm-secret"},
             paths={"ollama": "/api", "lmstudio": "/api/v1"})
        models = wait_models(ctx, lambda m: ids(m) == EXPECTED)
        ctx.rec("discovered", ids(models))
        record_lists(ctx, "lists")
        record_requests(ctx, servers, "requests")
    finally:
        close(servers)


@scenario("local", hold_for_title=0, timeout=240)
def local_chat(ctx):
    """a session on a discovered vLLM model sends OpenAI-compatible chat requests to the discovered base URL."""
    servers = start_servers()
    try:
        boot(ctx, servers, keys={"vllm": "vllm-secret"}, extra={"model": "vllm/Qwen/Qwen3-8B"})
        wait_models(ctx, lambda m: ids(m) == EXPECTED)
        sid = ctx.create_session(title="Local chat", model={"providerID": "vllm", "id": "Qwen/Qwen3-8B"})
        ctx.prompt(sid, "Say hello from the local model.")
        ctx.call("messages", "GET", f"/api/session/{sid}/message")
        ctx.rec("chat", servers["vllm"].seen(chat=True))
        ctx.rec("chat.mock", len(ctx.mock.main_requests()))
    finally:
        close(servers)


@scenario("local", hold_for_title=0, timeout=300)
def local_outage(ctx):
    """a server that goes down keeps its last inventory; the next answer after it returns replaces it (30 s refresh)."""
    servers = start_servers()
    try:
        boot(ctx, servers)
        ctx.rec("discovered", ids(wait_models(ctx, lambda m: ids(m) == EXPECTED)))
        for s in servers.values():
            s.down = True
            s.clear()
        # The next refresh (30 s after the first) fails on every server.
        deadline = time.time() + 45
        while time.time() < deadline and not all(s.requests for s in servers.values()):
            time.sleep(0.5)
        time.sleep(2)
        ctx.rec("down.asked", {name: bool(servers[name].requests) for name in PROVIDERS})
        ctx.rec("down.models", ids(local_models(ctx)))
        # Back with a different inventory: the refresh after that replaces it.
        servers["lmstudio"].models = [lm_model("new/model", "New Model", "llama", (), 8192)]
        servers["ollama"].models = []
        servers["vllm"].models = [vllm_model("Qwen/Qwen3-8B", 40960)]
        mark = len(ctx.server.events)
        for s in servers.values():
            s.down = False
        back = ["lmstudio/new/model", "vllm/Qwen/Qwen3-8B"]
        ctx.rec("back.models", ids(wait_models(ctx, lambda m: ids(m) == back, timeout=45)))
        time.sleep(1)
        record_lists(ctx, "back")
        updates(ctx, mark, "back.updates")
    finally:
        close(servers)


@scenario("local", hold_for_title=0, timeout=240)
def local_config_reload(ctx):
    """a configuration change moves a provider to another server and key: the old inventory goes, the new one comes."""
    servers = start_servers()
    other = LocalServer("lmstudio", [lm_model("other/model", "Other Model", "mistral", (4096,), 32768, False, True)],
                        name="lmstudio-other")
    try:
        boot(ctx, servers, watch=True)
        ctx.rec("discovered", ids(wait_models(ctx, lambda m: ids(m) == EXPECTED)))
        time.sleep(1)
        path = os.path.join(ctx.project, "opencode.json")
        cfg = json.load(open(path))
        cfg["providers"]["lmstudio"]["settings"] = {"baseURL": other.origin + "/v1", "apiKey": "other-key"}
        # Removing the vLLM endpoint falls back to the default origin, which the harness's global config replaces.
        del cfg["providers"]["vllm"]
        mark = len(ctx.server.events)
        with open(path, "w") as f:
            f.write(json.dumps(cfg, indent=2))
        ctx.rec("config.updated", ctx.wait_for("config.updated", start=mark, timeout=15) is not None)
        expected = ["lmstudio/other/model", "ollama/gemma3:4b", "ollama/llama3.2:3b"]
        ctx.rec("reloaded", ids(wait_models(ctx, lambda m: ids(m) == expected)))
        time.sleep(1.5)
        record_lists(ctx, "reloaded")
        ctx.rec("other.requests", other.seen())
        updates(ctx, mark, "reload.updates")
    finally:
        other.close()
        close(servers)
