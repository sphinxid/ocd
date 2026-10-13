"""The built-in GitHub Copilot provider plugin (core/src/plugin/provider/github-copilot.ts, core/src/github-copilot)
against the mock as a Copilot API: a fake OAuth credential whose `apiEndpoint` metadata points at the mock (the
same credential for both binaries, created through the API in the isolated store), the live `/models` sync, the
catalog it derives and the requests of each route (upstream's in-tree AI SDK provider for `/responses` and
`/chat/completions`, the native Anthropic package for `/v1/messages`), plus the utility-model title.

No request may leave the machine: HTTPS goes to a dead proxy, so a regression cannot reach GitHub."""
import os

from parity.registry import scenario
from parity.plugins import BUN

# Mock Copilot `/models` answer (the shape of api.githubcopilot.com/models, API version 2026-08-01).


def _remote(id, name, family, endpoints=None, version=None, picker=True, efforts=None, adaptive=None,
            max_think=None, vision=None, media=None, prices=None, policy=None, tools=True):
    caps = {"family": family,
            "limits": {"max_context_window_tokens": 200000, "max_output_tokens": 64000, "max_prompt_tokens": 128000},
            "supports": {"streaming": True}}
    if tools is not None:
        caps["supports"]["tool_calls"] = tools
    if media:
        caps["limits"]["vision"] = {"max_prompt_image_size": 3145728, "max_prompt_images": 5,
                                    "supported_media_types": media}
    if efforts:
        caps["supports"]["reasoning_effort"] = efforts
    if adaptive is not None:
        caps["supports"]["adaptive_thinking"] = adaptive
    if max_think is not None:
        caps["supports"]["max_thinking_budget"] = max_think
        caps["supports"]["min_thinking_budget"] = 1024
    if vision is not None:
        caps["supports"]["vision"] = vision
    model = {"model_picker_enabled": picker, "id": id, "name": name, "version": version or id, "capabilities": caps,
             "object": "model", "vendor": "mock"}
    if endpoints is not None:
        model["supported_endpoints"] = endpoints
    if prices:
        model["billing"] = {"token_prices": {"batch_size": prices[0], "default": prices[1]}}
    if policy:
        model["policy"] = {"state": policy}
    return model


MODELS = {"object": "list", "data": [
    _remote("gpt-5.4", "GPT-5.4", "gpt-5.4", ["/responses", "/chat/completions"], version="gpt-5.4-2026-03-05",
            efforts=["low", "medium", "high"], vision=True, media=["image/png", "image/jpeg"],
            prices=[1000000, {"input_price": 1.25, "output_price": 10, "cache_read_price": 0.125}]),
    _remote("claude-sonnet-4.6", "Claude Sonnet 4.6", "claude-sonnet-4.6", ["/v1/messages", "/chat/completions"],
            efforts=["low", "medium", "high"], adaptive=True, vision=True, media=["image/png", "application/pdf"],
            prices=[2000000, {"input_price": 3, "output_price": 15, "cache_price": 0.3}]),
    _remote("claude-haiku-4.5", "Claude Haiku 4.5", "claude-haiku-4.5", ["/v1/messages"], max_think=32000,
            version="2025-10-01"),
    _remote("gemini-3.5-flash", "Gemini 3.5 Flash", "gemini-3.5-flash", ["/chat/completions"], vision=False,
            media=["image/png"]),
    _remote("gpt-4o-mini", "GPT-4o mini", "gpt-4o-mini", ["/chat/completions"], picker=False),
    _remote("grok-4.6", "Grok 4.6", "grok-4.6"),
    _remote("gpt-5-chat-latest", "GPT-5 chat", "gpt-5", ["/chat/completions"]),
    # Remote-only models: release dates from `Date.parse(version)` (an ISO date, and a legacy-format string).
    _remote("mai-code-2", "MAI Code 2", "mai-code", ["/responses"], version="2026-01-15"),
    _remote("oswe-4.6", "OSWE 4.6", "oswe"),
    _remote("o-disabled", "Disabled", "x", ["/chat/completions"], policy="disabled"),
    {"id": "broken-entry", "name": "no capabilities"},
    _remote("no-tools", "No tools", "x", ["/chat/completions"], tools=None),
]}

SAFE_ENV = {"HTTPS_PROXY": "http://127.0.0.1:9", "https_proxy": "http://127.0.0.1:9",
            "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost"}

TRANSPORT_HEADERS = ("content-length", "host", "connection", "accept-encoding", "accept")


def setup(ctx, model, synced):
    ctx.env_extra.update(SAFE_ENV)
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    if synced:
        ctx.mock.routes["/models"] = (200, MODELS)
    ctx.set_script([
        {"match": "read hello", "tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": ["The file ", "says hi."], "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
        {"text": "Second answer."},
        {"text": "Third answer."},
    ])
    ctx.make_project(files={"hello.txt": "hi\n"}, config_extra={"model": model})
    ctx.start()


def connect(ctx):
    """The fake Copilot OAuth credential (`apiEndpoint` = the mock), then wait for the plugin's reload."""
    status, _ = ctx.call("credential.create", "POST", "/api/credential", {
        "integrationID": "github-copilot",
        "value": {"type": "oauth", "methodID": "device", "refresh": "gho_parity_refresh", "access": "gho_parity_access",
                  "expires": 0, "metadata": {"apiEndpoint": f"http://127.0.0.1:{ctx.mock.port}"}}}, record=False)
    ctx.rec("credential.create", status)


def copilot_models(ctx, wait_for):
    """The `github-copilot` catalog once it shows `wait_for` (the synced or the base-URL catalog), sorted by id."""
    import time
    deadline = time.time() + 15
    rows = []
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", "/api/model", headers={"x-opencode-directory": ctx.project})
        rows = [m for m in (data or {}).get("data", []) if m.get("providerID") == "github-copilot"]
        if wait_for(rows):
            break
        time.sleep(0.2)
    return sorted(rows, key=lambda m: m["id"])


def record_requests(ctx):
    """Everything the binary sent to the mock Copilot API: the model sync, titles, then the model requests in
    order. Header names are lowercased and sorted (the native routes' HTTP client does not keep case or order);
    transport headers are dropped."""
    order = {"GET": 0, "title": 1, "main": 2}
    out = []
    for r in sorted(ctx.mock.requests, key=lambda r: (order.get(r["kind"], 3), r["at"])):
        headers = {k.lower(): v for k, v in r["headers"].items() if k.lower() not in TRANSPORT_HEADERS}
        out.append({"kind": r["kind"], "path": r["path"], "headers": dict(sorted(headers.items())), "body": r["body"]})
    ctx.rec("requests", out)


def converse(ctx, models):
    """One prompt per model (the first with a tool call), then the transcript and the session title."""
    sid = ctx.create_session()
    for i, model in enumerate(models):
        if i:
            ctx.call(f"session.model.{model}", "POST", f"/api/session/{sid}/model",
                     {"model": {"providerID": "github-copilot", "id": model}})
        ctx.prompt(sid, "read hello" if i == 0 else f"prompt {i + 1}", timeout=90)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    _, info = ctx.call("session", "GET", f"/api/session/{sid}", record=False)
    ctx.rec("session.title", ((info or {}).get("data") or {}).get("title"))


@scenario("plugin", hold_for_title=0, timeout=300)
def plugin_copilot(ctx):
    """GitHub Copilot with a synced model list: the derived catalog (native Anthropic for `/v1/messages`, the in-tree
    AI SDK provider with the advertised endpoint otherwise), `/responses`, `/chat/completions` and `/v1/messages`
    requests with the Copilot headers, and the utility-model title."""
    setup(ctx, "github-copilot/gpt-5.4", synced=True)
    connect(ctx)
    ctx.rec("models", copilot_models(ctx, lambda rows: any(m["id"] == "oswe-4.6" for m in rows)))
    converse(ctx, ["gpt-5.4", "gemini-3.5-flash", "claude-sonnet-4.6"])
    record_requests(ctx)


@scenario("plugin", hold_for_title=0, timeout=300)
def plugin_copilot_unsynced(ctx):
    """GitHub Copilot whose model sync fails (404): every models.dev model through the in-tree AI SDK provider at the
    connection's API base, routed by model id (GPT-5+ to `/responses`, the rest to `/chat/completions`)."""
    setup(ctx, "github-copilot/gpt-5.4", synced=False)
    connect(ctx)
    base = f"http://127.0.0.1:{ctx.mock.port}"
    rows = copilot_models(ctx, lambda rows: rows and all((m.get("settings") or {}).get("baseURL") == base for m in rows))
    ctx.rec("models", [{k: m.get(k) for k in ("id", "package", "settings", "enabled")} for m in rows])
    converse(ctx, ["gpt-5.4", "claude-sonnet-4.6"])
    record_requests(ctx)
