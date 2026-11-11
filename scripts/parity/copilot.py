"""The built-in GitHub Copilot provider plugin (core/src/plugin/provider/github-copilot.ts, core/src/github-copilot)
against the mock as a Copilot API: a fake OAuth credential whose `apiEndpoint` metadata points at the mock (the
same credential for both binaries, created through the API in the isolated store), the live `/models` sync, the
catalog it derives and the requests of each route (since 2.2.7 all native: OpenAI-compatible Responses for
`/responses`, OpenAI-compatible Chat for `/chat/completions`, Anthropic for `/v1/messages`), the utility-model
title, and Copilot Auto (the `auto` model, the `/auto` router, the per-session pick and its session token).

No request may leave the machine: HTTPS goes to a dead proxy, so a regression cannot reach GitHub. The account
lookup (`https://api.github.com/copilot_internal/user`) therefore fails in both binaries, after its retries, and the
stored `apiEndpoint` serves."""
import os

from parity.registry import scenario
from parity.plugins import BUN

# Mock Copilot `/models` answer (the shape of api.githubcopilot.com/models, API version 2026-08-01).


def _remote(id, name, family, endpoints=None, version=None, picker=True, efforts=None, adaptive=None,
            max_think=None, vision=None, media=None, prices=None, policy=None, tools=True, auto=None, fallback=None):
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
    if prices or auto is not None:
        model["billing"] = {}
    if prices:
        model["billing"]["token_prices"] = {"batch_size": prices[0], "default": prices[1]}
    if auto is not None:
        model["billing"]["auto_discount"] = auto
    if fallback is not None:
        model["is_chat_fallback"] = fallback
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


# A free plan's list: every model hidden from the picker, an Auto pool and a chat fallback.
AUTO_MODELS = {"object": "list", "data": [
    _remote("gpt-5.4", "GPT-5.4", "gpt-5.4", ["/responses", "/chat/completions"], picker=False, auto=0.1,
            efforts=["low", "medium", "high"]),
    _remote("claude-haiku-4.5", "Claude Haiku 4.5", "claude-haiku-4.5", ["/v1/messages"], picker=False, auto=0.1),
    _remote("gemini-3.5-flash", "Gemini 3.5 Flash", "gemini-3.5-flash", ["/chat/completions"], picker=False,
            fallback=True),
    _remote("gpt-4o-mini", "GPT-4o mini", "gpt-4o-mini", ["/chat/completions"], picker=False),
]}


def setup(ctx, model, synced, models=None):
    ctx.env_extra.update(SAFE_ENV)
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    if synced:
        ctx.mock.routes["/models"] = (200, models or MODELS)
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
    # The account lookup fails through the dead proxy after its retries (1, 2 and 4 seconds) before the list loads.
    deadline = time.time() + 40
    rows = []
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", "/api/model", headers={"x-opencode-directory": ctx.project})
        rows = [m for m in (data or {}).get("data", []) if m.get("providerID") == "github-copilot"]
        if wait_for(rows):
            break
        time.sleep(0.2)
    return sorted(rows, key=lambda m: m["id"])


def record_requests(ctx):
    """Everything the binary sent to the mock Copilot API: the model syncs, titles, then the model requests in
    order. Header names are lowercased and sorted (the native routes' HTTP client does not keep case or order);
    transport headers are dropped.

    The credential's switch syncs the model list at every loaded location running the plugin: the project and the
    global config location credential creation boots, so two identical syncs. Upstream sometimes sends a third:
    that location's plugin also syncs when it finishes activating, and whether its switch subscription exists by
    then is a race. Identical syncs are recorded once, with whether there were at least two."""
    order = {"GET": 0, "title": 1, "main": 2, "POST": 3}
    out = []
    syncs = 0
    for r in sorted(ctx.mock.requests, key=lambda r: (order.get(r["kind"], 3), r["at"])):
        headers = {k.lower(): v for k, v in r["headers"].items() if k.lower() not in TRANSPORT_HEADERS}
        entry = {"kind": r["kind"], "path": r["path"], "headers": dict(sorted(headers.items())), "body": r["body"]}
        if r["kind"] == "GET":
            syncs += 1
            if entry in out:
                continue
        out.append(entry)
    ctx.rec("syncs.at.least.two", syncs >= 2)
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
    """GitHub Copilot with a synced model list: the derived catalog (native Anthropic for `/v1/messages`, the native
    OpenAI-compatible Responses or Chat package for the advertised endpoint otherwise), `/responses`,
    `/chat/completions` and `/v1/messages` requests with the Copilot headers, and the utility-model title."""
    setup(ctx, "github-copilot/gpt-5.4", synced=True)
    connect(ctx)
    ctx.rec("models", copilot_models(ctx, lambda rows: any(m["id"] == "oswe-4.6" for m in rows)))
    converse(ctx, ["gpt-5.4", "gemini-3.5-flash", "claude-sonnet-4.6"])
    record_requests(ctx)


@scenario("plugin", hold_for_title=0, timeout=300)
def plugin_copilot_auto(ctx):
    """GitHub Copilot Auto on a free plan's list: the `auto` model (the pool's smallest window), the `/auto` router
    call with the last user prompt, the pick kept for the session with its `Copilot-Session-Token`, a new route after
    a compaction, and the chat fallback when the router fails."""
    picks = [
        (200, {"session_token": "auto-token-1", "expires_at": 4000000000, "selected_model": {"id": "gpt-5.4"}}),
        (200, {"session_token": "auto-token-2", "expires_at": 4000000000, "selected_model": {"id": "claude-haiku-4.5"}}),
        (503, {"error": "unavailable"}),
    ]
    ctx.mock.post_routes["/auto"] = lambda body: picks.pop(0) if len(picks) > 1 else picks[0]
    setup(ctx, "github-copilot/auto", synced=True, models=AUTO_MODELS)
    ctx.set_script([
        {"match": "read hello", "tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": ["The file ", "says hi."], "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
        {"text": "Second answer."},
        {"text": "Compacted summary."},
        {"text": "Third answer."},
        {"text": "Fallback answer."},
    ])
    connect(ctx)
    ctx.rec("models", copilot_models(ctx, lambda rows: any(m["id"] == "auto" for m in rows)))
    sid = ctx.create_session()
    ctx.call("session.model.auto", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "github-copilot", "id": "auto"}})
    ctx.prompt(sid, "read hello", timeout=90)
    ctx.prompt(sid, "prompt 2", timeout=90)
    mark = len(ctx.server.events)
    ctx.call("compact", "POST", f"/api/session/{sid}/compact", {})
    ctx.wait_idle(sid, start=mark, timeout=90)
    ctx.prompt(sid, "prompt 3", timeout=90)
    child = ctx.create_session(label="session.child", parentID=sid)
    ctx.call("session.child.model.auto", "POST", f"/api/session/{child}/model", {"model": {"providerID": "github-copilot", "id": "auto"}})
    ctx.prompt(child, "prompt 4", timeout=90)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("messages.child", "GET", f"/api/session/{child}/message")
    record_requests(ctx)


@scenario("plugin", hold_for_title=0, timeout=300)
def plugin_copilot_unsynced(ctx):
    """GitHub Copilot whose model sync fails (404): every models.dev model on a native package at the connection's API
    base, routed by its models.dev package (Anthropic to `/v1/messages`) or else by model id (GPT-5+, Grok and MAI
    Code to `/responses`, the rest to `/chat/completions`)."""
    setup(ctx, "github-copilot/gpt-5.4", synced=False)
    connect(ctx)
    base = f"http://127.0.0.1:{ctx.mock.port}"
    rows = copilot_models(ctx, lambda rows: rows and all((m.get("settings") or {}).get("baseURL") == base for m in rows))
    ctx.rec("models", [{k: m.get(k) for k in ("id", "package", "settings", "enabled")} for m in rows])
    converse(ctx, ["gpt-5.4", "claude-sonnet-4.6"])
    record_requests(ctx)
