"""opencode 2.4.1 provider wire changes: Google Interactions responses continued across requests (#65006) with its
safety settings (#65036), undeclared replayed tool names in provider-safe form (#65292), OpenAI's `cyber_policy` and
Gemini's terms-of-use refusals as content policy (#64844, #64910) and the ChatGPT workspace usage limit's own type
(#64926)."""
from parity.loop import run_prompt
from parity.registry import scenario
from parity.wire22 import failing, provider


def interactions(port):
    return {"model": "gmock/gemini-test", "providers": {"gmock": {
        "name": "Google Mock", "package": "@opencode/ai/providers/google/interactions",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1beta", "apiKey": "test"},
        "models": {"gemini-test": {"name": "Gemini Test", "limit": {"context": 100000, "output": 4096},
                                   "settings": {"safetySettings": [
                                       {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_LOW_AND_ABOVE"}]}}}}}}


@scenario("wire241", hold_for_title=0)
def wire241_interactions_continuation(ctx):
    """A Google Interactions response split across two requests: the second request resends the body with the
    checkpoint's `continuation_token`, the text joins into one answer and the usage sums the hops."""
    run_prompt(ctx, [
        {"match": "user_input", "raw": [
            {"event_type": "step.start", "index": 0, "step": {"type": "model_output"}},
            {"event_type": "step.delta", "index": 0, "delta": {"type": "text", "text": "One"}},
            {"event_type": "step.stop", "index": 0},
            {"event_type": "interaction.completed", "interaction": {
                "id": "hop-1", "status": "requires_continuation", "continuation_token": "next-hop",
                "usage": {"total_input_tokens": 10, "total_output_tokens": 3, "total_tokens": 13}}},
        ]},
        {"match": "next-hop", "raw": [
            {"event_type": "step.start", "index": 0, "step": {"type": "model_output"}},
            {"event_type": "step.delta", "index": 0, "delta": {"type": "text", "text": ", two."}},
            {"event_type": "step.stop", "index": 0},
            {"event_type": "interaction.completed", "interaction": {
                "id": "hop-2", "status": "completed",
                "usage": {"total_input_tokens": 14, "total_output_tokens": 2, "total_tokens": 16}}},
        ]},
    ], llm_full=True, config_extra=interactions(ctx.mock.port))


@scenario("wire241")
def wire241_undeclared_tool_name(ctx):
    """The model calls a tool no definition declares, with a dotted name: the replay sends the call under a name
    OpenAI Chat accepts."""
    run_prompt(ctx, [
        {"tools": [{"name": "opencode.session.search", "args": {}, "id": "call_1"}]},
        {"text": "Done."},
    ], llm_full=True, config_extra=provider(ctx.mock.port))


@scenario("wire241")
def wire241_cyber_policy(ctx):
    """OpenAI's cybersecurity safeguard refuses the request: a content-policy session error."""
    message = "This content was flagged for possible cybersecurity risk. If this seems wrong, try rephrasing your request."
    failing(ctx, 400, {"error": {"message": message, "type": "invalid_request_error", "param": None, "code": "cyber_policy"}})


@scenario("wire241")
def wire241_terms_of_use(ctx):
    """Gemini's terms-of-use refusal (403) through an OpenAI-compatible host: content policy, not authentication."""
    failing(ctx, 403, {"error": {"code": 403, "message": "Request rejected in accordance with our terms of use and policies.",
                                 "status": "PERMISSION_DENIED"}})


@scenario("wire241")
def wire241_chatgpt_workspace_limit(ctx):
    """The ChatGPT workspace usage limit has its own session error type."""
    failing(ctx, 429, {"error": {"code": "subscription_sharing_workplace_individual_usage_limit_exceeded",
                                 "message": "Limit"}})
