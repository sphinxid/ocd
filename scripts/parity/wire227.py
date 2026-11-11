"""opencode 2.2.7 provider wire changes: unreachable hosts retry on their own budget and name the network in the
session error (Bun's system codes on a failed connect), a refused connection keeps the general retry allowance, and
ChatGPT's workspace usage-limit code. No request leaves the machine: `.invalid` never resolves, and everything else
not on loopback goes to a dead proxy."""
from parity.registry import scenario
from parity.wire22 import failing


def unreachable(ctx, base_url):
    """A prompt to a provider at `base_url`: the first retry notice, then the user interrupts."""
    ctx.env_extra.update({"HTTPS_PROXY": "http://127.0.0.1:9", "HTTP_PROXY": "http://127.0.0.1:9",
                          "https_proxy": "http://127.0.0.1:9", "http_proxy": "http://127.0.0.1:9",
                          "NO_PROXY": "127.0.0.1,localhost,.invalid", "no_proxy": "127.0.0.1,localhost,.invalid"})
    ctx.set_script([])
    config = {"model": "mock/m1", "providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": base_url, "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}}}}
    ctx.make_project(config=config)
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    mark = len(ctx.server.events)
    ctx.prompt(sid, "hello", wait=False)
    retry = ctx.wait_for("session.retry.scheduled", start=mark, timeout=30)
    data = (retry or {}).get("data", {})
    ctx.rec("retry.scheduled", {"attempt": data.get("attempt"), "error": data.get("error")})
    ctx.call("interrupt", "POST", f"/api/session/{sid}/interrupt")
    ctx.wait_idle(sid, start=mark)


@scenario("wire227", hold_for_title=0)
def wire227_unreachable_host(ctx):
    """A host name that does not resolve: `provider.transport` "Can't reach <host>. Check the network connection.
    (getaddrinfo ENOTFOUND <host>)", retried on the unreachable-host schedule."""
    unreachable(ctx, "http://nonexistent-host-ocd-test.invalid/v1")


@scenario("wire227", hold_for_title=0)
def wire227_refused_host(ctx):
    """A refused connection: Bun's `ECONNREFUSED` message, on the general retry allowance."""
    unreachable(ctx, "http://127.0.0.1:1/v1")


@scenario("wire227")
def wire227_chatgpt_workspace_limit(ctx):
    """ChatGPT's workspace usage limit names the admin's limit (#2.2.7)."""
    failing(ctx, 429, {"error": {"code": "subscription_sharing_workplace_individual_usage_limit_exceeded",
                                 "message": "Limit"}})


@scenario("wire227", hold_for_title=0)
def wire227_azure_messages(ctx):
    """Azure Messages for Foundry Claude (`@opencode/ai/providers/azure/messages`): Anthropic Messages at the
    configured base with the key as `x-api-key` (no `api-key`), and its `queryParams`."""
    ctx.set_script([{"text": "Hola."}])
    ctx.make_project(config={"model": "foundry/claude-haiku-4-5", "providers": {"foundry": {
        "name": "Foundry", "package": "@opencode/ai/providers/azure/messages",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/anthropic/v1/", "apiKey": "foundry-key",
                     "queryParams": {"trace": "1"}},
        "models": {"claude-haiku-4-5": {"name": "Claude Haiku 4.5", "limit": {"context": 200000, "output": 32000}}}}}})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "hola")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    out = []
    for r in ctx.mock.main_requests():
        h = {k.lower(): v for k, v in r["headers"].items()}
        out.append({"path": r["path"], "x-api-key": h.get("x-api-key"), "api-key": h.get("api-key"),
                    "authorization": h.get("authorization"), "anthropic-version": h.get("anthropic-version"),
                    "model": (r["body"] or {}).get("model"), "max_tokens": (r["body"] or {}).get("max_tokens")})
    ctx.rec("requests", out)
