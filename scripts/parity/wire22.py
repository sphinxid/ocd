"""opencode 2.2 provider wire changes: usage-limit error types, reported costs, output headroom, shared chat options,
OpenAI Chat finish handling and throttling that is retried instead of compacted."""
from parity.loop import run_prompt
from parity.registry import scenario


def provider(port, settings=None, model=None):
    m1 = {"name": "Mock One", "limit": {"context": 100000, "output": 4096},
          "cost": {"input": 3, "output": 15, "cache": {"read": 0.3, "write": 3.75}}}
    m1.update(model or {})
    return {"providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test", **(settings or {})},
        "models": {"m1": m1}}}}


def failing(ctx, status, body, headers=None):
    turn = {"status": status, "body": body}
    if headers:
        turn["headers"] = headers
    run_prompt(ctx, [turn], config_extra=provider(ctx.mock.port), before=lambda ctx, sid: None)


@scenario("wire22")
def wire22_go_limit(ctx):
    """OpenCode Go refuses with its usage limit: provider.go-limit names the limit."""
    failing(ctx, 429, {"type": "error", "error": {"type": "GoUsageLimitError", "message": "Go usage limit exceeded"},
                       "metadata": {"workspace": "wrk_go_limit", "limitName": "5 hour"}}, {"retry-after": "120"})


@scenario("wire22")
def wire22_model_budget_limit(ctx):
    """A Console budget on the requested model: provider.model-budget-limit."""
    failing(ctx, 429, {"type": "error",
                       "error": {"type": "BudgetLimitError", "message": "A budget policy for this model has been reached."},
                       "metadata": {"coverage": "models", "resetsAt": "2026-11-01T00:00:00.000Z"}})


@scenario("wire22")
def wire22_budget_limit(ctx):
    """A Console budget pausing all model use, in Google's envelope: provider.budget-limit."""
    failing(ctx, 429, {"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "Policy budget exceeded",
                                 "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                                              "reason": "BudgetLimitError", "domain": "opencode.ai",
                                              "metadata": {"coverage": "all", "resetsAt": "2026-11-01T00:00:00.000Z"}}]}})


@scenario("wire22")
def wire22_chatgpt_limit(ctx):
    """The ChatGPT usage-limit code types the failure whatever its status: provider.chatgpt-limit."""
    failing(ctx, 400, {"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "Limit"}})


@scenario("wire22")
def wire22_reported_cost(ctx):
    """usage.cost reported by the endpoint replaces the catalog price."""
    run_prompt(ctx, [{"text": "Hello", "usage": {"prompt_tokens": 1000, "completion_tokens": 100, "cost": 0.25}}],
               config_extra=provider(ctx.mock.port))


@scenario("wire22")
def wire22_reported_cost_zero(ctx):
    """A reported cost of 0 is authoritative; a malformed one falls back to the catalog price."""
    run_prompt(ctx, [{"tools": [{"name": "read", "args": {"path": "$PROJECT/a.txt"}}],
                      "usage": {"prompt_tokens": 1000, "completion_tokens": 100, "cost": 0}},
                     {"text": "Done", "usage": {"prompt_tokens": 1000, "completion_tokens": 100, "cost": "lots"}}],
               files={"a.txt": "hi\n"}, config_extra=provider(ctx.mock.port))


@scenario("wire22")
def wire22_output_headroom(ctx):
    """max_tokens of a primary request leaves 1% of the window unused."""
    run_prompt(ctx, [{"text": "Hello"}], llm_full=True,
               config_extra=provider(ctx.mock.port, model={"limit": {"context": 30000, "output": 32000}}))


@scenario("wire22")
def wire22_chat_options(ctx):
    """Shared OpenAI options from provider settings reach the chat body."""
    run_prompt(ctx, [{"text": "Hello"}], llm_full=True, config_extra=provider(ctx.mock.port, settings={
        "serviceTier": "flex", "textVerbosity": "low", "parallelToolCalls": False, "safetyIdentifier": "user-hash",
        "metadata": {"source": "parity"}}))


@scenario("wire22")
def wire22_unknown_finish(ctx):
    """An unknown finish_reason maps to unknown instead of failing the response."""
    run_prompt(ctx, [{"raw": [
        {"id": "c1", "choices": [{"index": 0, "delta": {"content": "Hello"}}]},
        {"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "future_reason"}],
         "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}},
        "[DONE]"]}], config_extra=provider(ctx.mock.port))


@scenario("wire22")
def wire22_text_after_finish(ctx):
    """Text that arrives after the finish_reason keeps streaming."""
    run_prompt(ctx, [{"raw": [
        {"id": "c1", "choices": [{"index": 0, "delta": {"content": "Hel"}, "finish_reason": "stop"}]},
        {"id": "c1", "choices": [{"index": 0, "delta": {"content": "lo"}}],
         "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}},
        "[DONE]"]}], config_extra=provider(ctx.mock.port))


@scenario("wire22")
def wire22_throttle_not_overflow(ctx):
    """Bedrock-style "Too many tokens, please wait" on 429 is retried, not compacted."""
    run_prompt(ctx, [
        {"status": 429, "headers": {"retry-after": "1"},
         "body": {"message": "Too many tokens, please wait before trying again."}},
        {"text": "Recovered."},
    ], config_extra=provider(ctx.mock.port), timeout=30)


@scenario("wire22")
def wire22_mcp_malformed_tools(ctx):
    """An MCP server listing malformed tools keeps its valid ones (fake_mcp.py)."""
    import os
    import sys
    server = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_mcp.py")
    extra = provider(ctx.mock.port)
    extra["mcp"] = {"servers": {"fake": {"type": "local", "command": [sys.executable, server]}}}
    run_prompt(ctx, [{"text": "Hello"}], llm_full=True, config_extra=extra,
               before=lambda ctx, sid: ctx.call("mcp.list", "GET", "/api/mcp?" + "directory=" + ctx.project))
